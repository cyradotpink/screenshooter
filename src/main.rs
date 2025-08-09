#![feature(try_blocks)]

use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use dbus::arg::RefArg as _;
use dbus::channel::Sender as _;

use pipewire::spa;

type DbusBoxDynVariant = dbus::arg::Variant<Box<dyn dbus::arg::RefArg>>;
type DbusBoxDynMap = HashMap<String, DbusBoxDynVariant>;

type DbusRefDynVariant<'a> = dbus::arg::Variant<&'a dyn dbus::arg::RefArg>;
type DbusRefDynMap<'a> = HashMap<String, DbusRefDynVariant<'a>>;

fn new_screen_cast_call(method: &str) -> Result<dbus::Message, anyhow::Error> {
    dbus::Message::new_method_call(
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
        method,
    )
    .map_err(|e| anyhow!(e))
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

fn get_next_portal_response(
    conn: &dbus::blocking::Connection,
    timeout: Duration,
) -> Result<DbusBoxDynMap, anyhow::Error> {
    let at_begin = Instant::now();
    let res_msg = loop {
        let remaining_timeout = timeout - (Instant::now() - at_begin);
        let msg = conn
            .channel()
            .blocking_pop_message(remaining_timeout)?
            .ok_or(anyhow!("Didn't receive response"))?;
        if msg.msg_type() == dbus::MessageType::Signal
            && msg.interface().as_deref() == Some("org.freedesktop.portal.Request")
            && msg.member().as_deref() == Some("Response")
        {
            break msg;
        }
    };
    let (response, results) = res_msg.read2::<u32, DbusBoxDynMap>()?;
    if response != 0 {
        return Err(anyhow!("Non-zero response code {response}"));
    }
    Ok(results)
}

fn get_next_dbus_reply(
    conn: &dbus::blocking::Connection,
    timeout: Duration,
) -> Result<dbus::Message, anyhow::Error> {
    let at_begin = Instant::now();
    let res_msg = loop {
        let remaining_timeout = timeout - (Instant::now() - at_begin);
        let msg = conn
            .channel()
            .blocking_pop_message(remaining_timeout)?
            .ok_or(anyhow!("Didn't receive reply"))?;
        if msg.msg_type() == dbus::MessageType::Error {
            return Err(anyhow!("Received error message: {msg:?}"));
        }
        if msg.msg_type() == dbus::MessageType::MethodReturn {
            break msg;
        }
    };
    Ok(res_msg)
}

fn get_pipewire_stream(conn: &dbus::blocking::Connection) -> Result<(u32, OwnedFd), anyhow::Error> {
    let mut req_options = DbusBoxDynMap::new();
    req_options.insert(
        "session_handle_token".to_owned(),
        dbus::arg::Variant(Box::new("1".to_owned())),
    );
    let req_msg = new_screen_cast_call("CreateSession")?.append1(req_options);
    conn.send(req_msg)
        .map_err(|_| anyhow!("Failed to send message"))?;
    let results = get_next_portal_response(conn, Duration::from_secs(1))?;
    let session_handle = results
        .get("session_handle")
        .and_then(|v| v.as_str())
        .ok_or(anyhow!("no session handle in response"))?;
    let session_handle = dbus::Path::from_slice(session_handle).map_err(|e| anyhow!(e))?;

    let mut req_options = DbusBoxDynMap::new();
    req_options.insert("types".to_owned(), dbus::arg::Variant(Box::new(2u32))); // window
    let req_msg = new_screen_cast_call("SelectSources")?.append2(&session_handle, req_options);
    conn.send(req_msg)
        .map_err(|_| anyhow!("Failed to send message"))?;
    let _results = get_next_portal_response(conn, Duration::from_secs(1))?;

    let req_msg = new_screen_cast_call("Start")?.append3(&session_handle, "", DbusBoxDynMap::new());
    conn.send(req_msg)
        .map_err(|_| anyhow!("Failed to send message"))?;
    let results = get_next_portal_response(conn, Duration::from_secs(5 * 60))?;
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

    let req_msg =
        new_screen_cast_call("OpenPipeWireRemote")?.append2(&session_handle, DbusBoxDynMap::new());
    conn.send(req_msg)
        .map_err(|_| anyhow!("Failed to send message"))?;
    let res_msg = get_next_dbus_reply(conn, Duration::from_secs(1))?;
    let file: std::fs::File = res_msg.get1().ok_or(anyhow!("no fd in return"))?;
    Ok((node_id, file.into()))
}

#[derive(Debug, Clone)]
struct LatestFrame {
    data: Vec<u8>,
    info: spa::param::video::VideoInfoRaw,
    ts: Instant,
}
impl LatestFrame {
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
    dbus_conn: dbus::blocking::Connection,
    pw_loop: pipewire::main_loop::MainLoop,
    pw_ctx: pipewire::context::Context,
    pw_core: pipewire::core::Core,
    pw_stream: pipewire::stream::Stream,
    pw_listener: pipewire::stream::StreamListener<UserData>,
}
impl ScreenshooterSetup {
    fn new(
        latest_frame: Arc<Mutex<LatestFrame>>,
        quit: Arc<AtomicBool>,
    ) -> Result<Self, anyhow::Error> {
        let dbus_conn = dbus::blocking::Connection::new_session()?;
        let (node_id, fd) = get_pipewire_stream(&dbus_conn)?;

        let pw_loop = pipewire::main_loop::MainLoop::new(None)?;
        let pw_ctx = pipewire::context::Context::new(&pw_loop)?;
        let pw_core = pw_ctx.connect_fd(fd, None)?;
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
                Id,
                spa::param::video::VideoFormat::BGRA
            ),
            // spa::pod::property!(
            //     spa::param::format::FormatProperties::VideoFormat,
            //     Choice, Enum, Id,
            //     spa::param::video::VideoFormat::RGBx,
            //     spa::param::video::VideoFormat::BGRx,
            // ),
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
            dbus_conn,
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
    handle: thread::JoinHandle<()>,
    latest_frame: Arc<Mutex<LatestFrame>>,
    quit: Arc<AtomicBool>,
    quit_notify: Arc<Mutex<HashMap<thread::ThreadId, thread::Thread>>>,
}
impl Screenshooter {
    fn new() -> Result<Self, anyhow::Error> {
        let latest_frame = Arc::new(Mutex::new(LatestFrame {
            data: Vec::new(),
            info: spa::param::video::VideoInfoRaw::default(),
            ts: Instant::now(),
        }));
        let latest_frame_thread_ref = latest_frame.clone();
        let quit = Arc::new(AtomicBool::new(false));
        let quit_thread_ref = quit.clone();
        let quit_notify = Arc::new(Mutex::new(
            HashMap::<thread::ThreadId, thread::Thread>::new(),
        ));
        let quit_notify_thread_ref = quit_notify.clone();
        let init = Arc::new(OnceLock::<Mutex<Result<(), anyhow::Error>>>::new());
        let init_thread_ref = init.clone();
        let handle = thread::spawn(move || {
            let latest_frame = latest_frame_thread_ref;
            let quit = quit_thread_ref;
            let quit_notify = quit_notify_thread_ref;
            let init = init_thread_ref;
            let setup_result = ScreenshooterSetup::new(latest_frame, quit.clone());
            let (ok, init_result) = match setup_result {
                Ok(v) => (Some(v), Ok(())),
                Err(e) => (None, Err(e)),
            };
            init.set(Mutex::new(init_result)).unwrap();
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
        let mut result = Ok(());
        std::mem::swap(&mut *init.wait().try_lock().unwrap(), &mut result);

        Ok(Self {
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
}

fn sleep_or_wait_for_quit(shooter: &Screenshooter, mut timeout: Duration) {
    let mut before_park = Instant::now();
    loop {
        thread::park_timeout(timeout);
        if shooter.is_quit() {
            break;
        }
        let now = Instant::now();
        let diff = now - before_park;
        if diff >= timeout {
            break;
        }
        timeout -= diff;
        before_park = now;
    }
}

fn main() -> Result<(), anyhow::Error> {
    let shooter = Screenshooter::new()?;
    let cur_thread = thread::current();
    shooter
        .quit_notify
        .lock()
        .unwrap()
        .insert(cur_thread.id(), cur_thread);

    while !shooter.is_quit() {
        let mut frame = shooter.latest_frame.lock().unwrap().take();
        if frame.data.len() == 0 {
            sleep_or_wait_for_quit(&shooter, Duration::from_secs(5));
            continue;
        }
        for px in frame.data.chunks_mut(4) {
            px.swap(0, 2);
            // px[3] = 255;
        }
        let f = std::fs::File::create("screenshot.png").unwrap();
        let mut enc = png::Encoder::new(f, frame.info.size().width, frame.info.size().height);
        enc.set_color(png::ColorType::Rgba);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&frame.data).unwrap();
        writer.finish().unwrap();

        sleep_or_wait_for_quit(&shooter, Duration::from_secs(5));
    }

    Ok(())
}
