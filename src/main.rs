#![feature(try_blocks)]

mod dbus_util;

use std::collections::HashMap;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use dbus::arg::RefArg as _;
use pipewire::spa;

type DbusBoxDynVariant = dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>;
type DbusBoxDynMap = HashMap<String, DbusBoxDynVariant>;

type DbusRefDynVariant<'a> = dbus::arg::Variant<&'a dyn dbus::arg::RefArg>;
type DbusRefDynMap<'a> = HashMap<String, DbusRefDynVariant<'a>>;

#[allow(unused)]
mod source_types {
    pub const MONITOR: u32 = 1;
    pub const WINDOW: u32 = 2;
    pub const VIRTUAL: u32 = 4;
    pub const ALL: u32 = MONITOR | WINDOW | VIRTUAL;
}
#[allow(unused)]
mod device_types {
    pub const KEYBOARD: u32 = 1;
    pub const POINTER: u32 = 2;
    pub const TOUCHSCREEN: u32 = 4;
    pub const ALL: u32 = KEYBOARD | POINTER | TOUCHSCREEN;
}

struct DbusBoxDynMapBuilder(DbusBoxDynMap);
impl DbusBoxDynMapBuilder {
    fn new() -> Self {
        Self(DbusBoxDynMap::new())
    }
    fn with_owned<T: dbus::arg::RefArg + 'static>(mut self, key: String, value: T) -> Self {
        self.0.insert(key, dbus::arg::Variant(Box::new(value)));
        self
    }
    fn take(self) -> DbusBoxDynMap {
        self.0
    }
}

// Panics if memory can't be allocated for the message
fn new_screen_cast_call(method: &str) -> dbus::Message {
    dbus::Message::new_method_call(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
        method,
    )
    .unwrap()
}

// Panics if memory can't be allocated for the message
fn new_remote_desktop_call(method: &str) -> dbus::Message {
    dbus::Message::new_method_call(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.RemoteDesktop",
        method,
    )
    .unwrap()
}

fn array_to_dynmap<'a>(
    array: Box<dyn Iterator<Item = &'a dyn dbus::arg::RefArg> + 'a>,
) -> Option<DbusRefDynMap<'a>> {
    let mut array = array;
    let mut map = HashMap::new();
    while let Some(key) = array.next() {
        let value = array.next()?;
        map.insert(
            key.as_str()?.to_owned(),
            dbus::arg::Variant(value.as_iter()?.next()?),
        );
    }
    Some(map)
}

fn get_portal_response(
    conn: &mut dbus_util::Connection2,
    serial: u32,
    request_token: &str,
    timeout: Duration,
) -> Result<DbusBoxDynMap, anyhow::Error> {
    // We controlled the path of the request handle by passing a "token"
    // call, which will we be used as the last segment of the path, ...(1)
    let predicted_req_path = format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        conn.conn.unique_name()[1..].replace(".", "_"),
        request_token
    );

    // Note that we do *not* register a match rule with the message bus. The
    // Response signal is unicast, so we don't need to do that. We're just telling
    // the Connection2 to hold on to the message for us when it is received.
    let response_matcher = dbus_util::Matcher::new()
        .with_message_type(dbus::MessageType::Signal)
        .with_path(predicted_req_path)
        .with_interface("org.freedesktop.portal.Request".to_owned())
        .with_member("Response".to_owned());
    let id = conn.register_matcher(1, response_matcher);

    let begin_time = Instant::now();
    let reply_msg = get_dbus_reply(conn, serial, timeout)?;
    if reply_msg.msg_type() == dbus::MessageType::Error {
        return Err(anyhow!("Got error reply: {reply_msg:?}"));
    }

    // (1)... however, this mechanism didn't exist in all versions of the Desktop Portal
    // specification. Previously, callers relied on the method call returning the Request
    // handle *before* the Response signal fires. We maintain compatibility with this
    // mechanism by modifying our signal matcher here. Note that this doesn't help
    // if the Response signal fired on an unexpected path before the call return: Then
    // we've already missed the response, and the upcoming block_on_once_matcher_remove
    // call will time out instead.
    let actual_req_path = reply_msg
        .get1::<dbus::Path>()
        .ok_or(anyhow!("No path in call return"))?;
    conn.matchers.get_mut(&id).unwrap().1.path = Some(actual_req_path.to_string());

    let res_msg = conn
        .block_on_once_matcher_remove(id, timeout.saturating_sub(Instant::now() - begin_time))?
        .1
        .ok_or(anyhow!("Timed out waiting for response"))?;
    let (response, results) = res_msg.read2::<u32, DbusBoxDynMap>()?;
    if response != 0 {
        return Err(anyhow!("Non-zero response code {response}"));
    }
    Ok(results)
}

fn get_dbus_reply(
    conn: &mut dbus_util::Connection2,
    serial: u32,
    timeout: Duration,
) -> Result<dbus::Message, anyhow::Error> {
    let matcher = dbus_util::Matcher::new().with_reply_serial(serial);
    let id = conn.register_matcher(1, matcher);
    let (_, reply) = conn.block_on_once_matcher_remove(id, timeout)?;
    reply.ok_or(anyhow!("Timed out waiting fo reply"))
}

#[allow(unused)]
fn remote_desktop_type(
    conn: &mut dbus_util::Connection2,
    session_handle: &dbus::Path,
    text: &str,
) -> Result<(), anyhow::Error> {
    for ch in text.chars() {
        let mut call = new_remote_desktop_call("NotifyKeyboardKeysym");
        call.append_all((session_handle, DbusBoxDynMap::new(), ch as i32, 1u32));
        let serial = conn.send(call)?;
        get_dbus_reply(conn, serial, Duration::from_secs(1))?;
        let mut call = new_remote_desktop_call("NotifyKeyboardKeysym");
        call.append_all((session_handle, DbusBoxDynMap::new(), ch as i32, 0u32));
        let serial = conn.send(call)?;
        get_dbus_reply(conn, serial, Duration::from_secs(1))?;
    }
    Ok(())
}

type MakeCallFn = fn(&str) -> dbus::Message;

fn create_portal_session(
    conn: &mut dbus_util::Connection2,
    make_call: MakeCallFn,
) -> Result<dbus::Path<'static>, anyhow::Error> {
    let token = conn.get_token();
    let req_options = DbusBoxDynMapBuilder::new()
        .with_owned("handle_token".to_owned(), token.to_string())
        .with_owned(
            "session_handle_token".to_owned(),
            conn.get_token().to_string(),
        )
        .take();
    let req_msg = make_call("CreateSession").append1(req_options);
    let serial = conn.send(req_msg)?;
    let results = get_portal_response(conn, serial, &token.to_string(), Duration::from_secs(1))?;
    let session_handle = results
        .get("session_handle")
        .and_then(|v| v.as_str())
        .ok_or(anyhow!("no session handle in response"))?;
    dbus::Path::new(session_handle).map_err(|e| anyhow!(e))
}

fn start_portal_session(
    conn: &mut dbus_util::Connection2,
    session_handle: &dbus::Path,
    make_call: MakeCallFn,
) -> Result<DbusBoxDynMap, anyhow::Error> {
    let token = conn.get_token();
    let req_options = DbusBoxDynMapBuilder::new()
        .with_owned("handle_token".to_owned(), token.to_string())
        .take();
    let req_msg = make_call("Start").append3(session_handle, "", req_options);
    let serial = conn.send(req_msg)?;
    get_portal_response(conn, serial, &token.to_string(), Duration::from_secs(300))
}

fn screen_cast_select_sources(
    conn: &mut dbus_util::Connection2,
    session_handle: &dbus::Path,
    source_types: u32,
) -> Result<(), anyhow::Error> {
    let token = conn.get_token();
    let req_options = DbusBoxDynMapBuilder::new()
        .with_owned("handle_token".to_owned(), token.to_string())
        .with_owned("types".to_owned(), source_types)
        .take();
    let req_msg = new_screen_cast_call("SelectSources").append2(session_handle, req_options);
    let serial = conn.send(req_msg)?;
    get_portal_response(conn, serial, &token.to_string(), Duration::from_secs(1)).map(|_| ())
}

fn remote_desktop_select_devices(
    conn: &mut dbus_util::Connection2,
    session_handle: &dbus::Path,
    device_types: u32,
) -> Result<(), anyhow::Error> {
    let token = conn.get_token();
    let req_options = DbusBoxDynMapBuilder::new()
        .with_owned("handle_token".to_owned(), token.to_string())
        .with_owned("types".to_owned(), device_types)
        .take();
    let req_msg = new_remote_desktop_call("SelectDevices").append2(session_handle, req_options);
    let serial = conn.send(req_msg)?;
    get_portal_response(conn, serial, &token.to_string(), Duration::from_secs(1)).map(|_| ())
}

fn screen_cast_open_pipe_wire_remote(
    conn: &mut dbus_util::Connection2,
    session_handle: &dbus::Path,
) -> Result<dbus::Message, anyhow::Error> {
    let req_msg =
        new_screen_cast_call("OpenPipeWireRemote").append2(session_handle, DbusBoxDynMap::new());
    let serial = conn.send(req_msg)?;
    get_dbus_reply(conn, serial, Duration::from_secs(1))
}

fn extract_pipewire_stream_node(results: &DbusBoxDynMap) -> Result<u32, anyhow::Error> {
    let stream: Option<_> = try {
        results
            .get("streams")?
            .as_iter()?
            .next()? // Unpack variant
            .as_iter()?
            .next()? // Get first stream from array
            .as_iter()?
    };
    let mut stream = stream.ok_or(anyhow!("no stream in response"))?;
    let node_id = stream
        .next()
        .and_then(|v| v.as_u64())
        .ok_or(anyhow!("no ID in stream tuple"))? as u32;
    let _properties = stream
        .next()
        .and_then(|v| v.as_iter())
        .and_then(array_to_dynmap)
        .ok_or(anyhow!("no properties in stream tuple"))?;
    Ok(node_id)
}

fn init_screen_cast_session(
    conn: &mut dbus_util::Connection2,
    source_types: u32,
    device_types: Option<u32>,
) -> Result<(dbus::Path<'static>, u32), anyhow::Error> {
    let new_call = if device_types.is_some() {
        new_remote_desktop_call
    } else {
        new_screen_cast_call
    };
    let session_handle = create_portal_session(conn, new_call)?;
    if let Some(device_types) = device_types {
        remote_desktop_select_devices(conn, &session_handle, device_types)?;
    }
    screen_cast_select_sources(conn, &session_handle, source_types)?;
    let results = start_portal_session(conn, &session_handle, new_call)?;
    let node_id = extract_pipewire_stream_node(&results)?;
    Ok((session_handle, node_id))
}

#[derive(Debug, Clone)]
struct LatestFrame {
    data: Vec<u8>,
    info: spa::param::video::VideoInfoRaw,
    ts: Instant,
}
impl LatestFrame {
    // Take the frame's data (leaving it empty), copying only its info and timestamp.
    fn take(&mut self) -> Self {
        let mut out = Self {
            data: Vec::new(),
            info: self.info,
            ts: self.ts,
        };
        std::mem::swap(&mut self.data, &mut out.data);
        out
    }
}

struct UserData {
    video_info: spa::param::video::VideoInfoRaw,
    latest_frame: Arc<Mutex<LatestFrame>>,
    quit: Arc<AtomicBool>,
}

#[allow(unused)]
struct ScreenshooterSetup {
    pw_loop: pipewire::main_loop::MainLoop,
    pw_ctx: pipewire::context::Context,
    pw_core: pipewire::core::Core,
    pw_stream: pipewire::stream::Stream,
    pw_listener: pipewire::stream::StreamListener<UserData>,
}
impl ScreenshooterSetup {
    fn new(
        dbus_conn: &mut dbus_util::Connection2,
        session_handle: &dbus::Path,
        node_id: u32,
        latest_frame: Arc<Mutex<LatestFrame>>,
        quit: Arc<AtomicBool>,
    ) -> Result<Self, anyhow::Error> {
        let res_msg = screen_cast_open_pipe_wire_remote(dbus_conn, session_handle)?;
        let file: std::fs::File = res_msg.get1().ok_or(anyhow!("no fd in return"))?;

        let pw_loop = pipewire::main_loop::MainLoop::new(None)?;
        let pw_ctx = pipewire::context::Context::new(&pw_loop)?;
        let pw_core = pw_ctx.connect_fd(file.into(), None)?;
        let pw_stream = pipewire::stream::Stream::new(
            &pw_core,
            "capture",
            pipewire::properties::Properties::new(),
        )?;
        use pipewire::stream::StreamFlags;
        let pod_object = spa::pod::object! {
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(
                spa::param::format::FormatProperties::MediaType,
                Id,
                spa::param::format::MediaType::Video
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::MediaSubtype,
                Id,
                spa::param::format::MediaSubtype::Raw
            ),
            spa::pod::property!(
                spa::param::format::FormatProperties::VideoFormat,
                Choice, Enum, Id,
                spa::param::video::VideoFormat::RGBA,
                spa::param::video::VideoFormat::BGRA,
            ),
        };

        let pod_value = spa::pod::Value::Object(pod_object);
        let mut pod_bytes = Vec::new();
        let mut cursor = Cursor::new(&mut pod_bytes);
        let _ser_res = spa::pod::serialize::PodSerializer::serialize(&mut cursor, &pod_value)?;
        let pod = spa::pod::Pod::from_bytes(&pod_bytes).ok_or(anyhow!("Couldn't make pod"))?;

        let default_info = spa::param::video::VideoInfoRaw::default();
        let data = UserData {
            video_info: default_info,
            latest_frame,
            quit,
        };
        let pw_listener = pw_stream
            .add_local_listener_with_user_data(data)
            .param_changed(|stream, data, id, param| {
                let param_type = spa::param::ParamType::from_raw(id);
                if param_type != spa::param::ParamType::Format {
                    return;
                }
                let param = match param {
                    Some(p) => p,
                    None => {
                        stream.disconnect().unwrap();
                        return;
                    }
                };
                let mut info = spa::param::video::VideoInfoRaw::new();
                info.parse(param).expect("failed to parse video info");
                data.video_info = info;
            })
            .state_changed(move |_stream, data, _from, to| {
                if to == pipewire::stream::StreamState::Unconnected {
                    data.quit.store(true, atomic::Ordering::Release);
                }
            })
            .process(|stream, data| {
                let info = data.video_info;
                let mut buffer = match stream.dequeue_buffer() {
                    Some(b) => b,
                    None => return,
                };
                let datas = buffer.datas_mut();
                let data_bytes = match datas.get_mut(0).and_then(|v| v.data()) {
                    Some(d) => d,
                    None => return,
                };
                let now = Instant::now();
                {
                    let mut latest_frame = data.latest_frame.lock().unwrap();
                    latest_frame.ts = now;
                    latest_frame.info = info;
                    if latest_frame.data.len() < data_bytes.len() {
                        let mut data_copy = data_bytes.to_owned();
                        std::mem::swap(&mut data_copy, &mut latest_frame.data);
                    } else {
                        latest_frame.data.truncate(data_bytes.len());
                        latest_frame.data.copy_from_slice(data_bytes);
                    }
                };
            })
            .register()?;

        pw_stream.connect(
            spa::utils::Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )?;

        Ok(Self {
            pw_loop,
            pw_ctx,
            pw_core,
            pw_stream,
            pw_listener,
        })
    }
}

#[allow(unused)]
struct Screenshooter {
    dbus_conn: dbus_util::Connection2,
    session_handle: dbus::Path<'static>,
    handle: thread::JoinHandle<()>,
    latest_frame: Arc<Mutex<LatestFrame>>,
    quit: Arc<AtomicBool>,
    quit_notify: Arc<Mutex<HashMap<thread::ThreadId, thread::Thread>>>,
}
impl Screenshooter {
    fn new(source_types: u32, device_types: Option<u32>) -> Result<Self, anyhow::Error> {
        let dbus_conn = dbus::blocking::Connection::new_session()?;
        let mut dbus_conn = dbus_util::Connection2::new(dbus_conn);
        let (session_handle, node_id) =
            init_screen_cast_session(&mut dbus_conn, source_types, device_types)?;
        let dbus_conn = Arc::new(Mutex::new(dbus_conn));

        let latest_frame = Arc::new(Mutex::new(LatestFrame {
            data: Vec::new(),
            info: spa::param::video::VideoInfoRaw::default(),
            ts: Instant::now(),
        }));
        let quit = Arc::new(AtomicBool::new(false));
        let quit_notify = Arc::new(Mutex::new(
            HashMap::<thread::ThreadId, thread::Thread>::new(),
        ));
        let init = Arc::new(OnceLock::<Result<(), anyhow::Error>>::new());

        let thread_data = (
            dbus_conn.clone(),
            session_handle.clone(),
            latest_frame.clone(),
            quit.clone(),
            quit_notify.clone(),
            init.clone(),
            thread::current(),
        );
        let handle = thread::spawn(move || {
            let (dbus_conn, session_handle, latest_frame, quit, quit_notify, init, spawning_thread) =
                thread_data;
            let mut dbus_conn_lock = dbus_conn.try_lock().unwrap();
            let setup_result = ScreenshooterSetup::new(
                &mut dbus_conn_lock,
                &session_handle,
                node_id,
                latest_frame,
                quit.clone(),
            );
            drop(dbus_conn_lock);
            let (ok, init_result) = match setup_result {
                Ok(v) => (Some(v), Ok(())),
                Err(e) => (None, Err(e)),
            };
            let _ = init.set(init_result);
            drop(init);
            drop(dbus_conn);
            spawning_thread.unpark();
            let setup = match ok {
                Some(v) => Rc::new(v),
                None => return,
            };
            let idle_setup = setup.clone();
            let _idle_source = setup.pw_loop.loop_().add_idle(true, move || {
                let quit = quit.load(atomic::Ordering::Acquire);
                if quit {
                    idle_setup.pw_loop.quit();
                    for th in quit_notify.lock().unwrap().values() {
                        th.unpark()
                    }
                }
            });
            setup.pw_loop.run();
        });
        while Arc::strong_count(&init) > 1 {
            thread::park();
        }
        // Unwraps are okay because 1. the Arc is never cloned again after the spawned thread
        // drops its reference and 2. it only drops its reference after initialising the result
        Arc::into_inner(init).unwrap().into_inner().unwrap()?;
        let dbus_conn = Arc::into_inner(dbus_conn).unwrap().into_inner().unwrap();

        Ok(Self {
            dbus_conn,
            session_handle,
            handle,
            latest_frame,
            quit,
            quit_notify,
        })
    }
    #[allow(unused)]
    fn quit(&self) {
        self.quit.store(true, atomic::Ordering::Release);
    }
    fn is_quit(&self) -> bool {
        self.quit.load(atomic::Ordering::Acquire)
    }
    fn reqister_quit_notify(&self, thread: thread::Thread) {
        self.quit_notify.lock().unwrap().insert(thread.id(), thread);
    }
    #[allow(unused)]
    fn unregister_quit_notify(&self, thread: thread::Thread) {
        self.quit_notify.lock().unwrap().remove(&thread.id());
    }
}

// Repeatedly parks the current thread until the timeout is reached (then returns false) or shooter.is_quit() is
// true after an unpark (then returns true). This function does *not* ensure that that the current thread is
// actually unparked on shooter quit. You must register the thread in shooter.quit_notify yourself.
fn park_until_quit_w_timeout(shooter: &Screenshooter, mut timeout: Duration) -> bool {
    let mut before_park = Instant::now();
    loop {
        thread::park_timeout(timeout);
        if shooter.is_quit() {
            break true;
        }
        let now = Instant::now();
        let elapsed = now - before_park;
        if elapsed >= timeout {
            break false;
        }
        timeout -= elapsed;
        before_park = now;
    }
}

// Let the user select a window to capture, then try to grab the latest captured frame every 5 seconds
// and save it as screenshot.png.
fn main() -> Result<(), anyhow::Error> {
    let shooter = Screenshooter::new(source_types::ALL, None)?;
    shooter.reqister_quit_notify(thread::current());
    while !shooter.is_quit() {
        let mut frame = shooter.latest_frame.lock().unwrap().take();
        if frame.data.is_empty() {
            park_until_quit_w_timeout(&shooter, Duration::from_secs(5));
            continue;
        }
        if frame.info.format() == spa::param::video::VideoFormat::BGRA {
            for px in frame.data.chunks_mut(4) {
                px.swap(0, 2);
            }
        }
        let f = std::fs::File::create("screenshot.png").unwrap();
        let mut enc = png::Encoder::new(f, frame.info.size().width, frame.info.size().height);
        enc.set_color(png::ColorType::Rgba);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&frame.data).unwrap();
        writer.finish().unwrap();

        park_until_quit_w_timeout(&shooter, Duration::from_secs(5));
    }

    Ok(())
}
