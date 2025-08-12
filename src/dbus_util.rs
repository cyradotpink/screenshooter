use std::collections::HashMap;
use std::time::{Duration, Instant};

use dbus::channel::Sender;
use dbus::{Message, MessageType, blocking::Connection};

#[derive(Debug, Clone)]
pub struct Matcher {
    pub message_type: Option<MessageType>,
    pub reply_serial: Option<u32>,
    pub sender: Option<String>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
}
impl Matcher {
    pub fn new() -> Self {
        Self {
            message_type: None,
            reply_serial: None,
            sender: None,
            path: None,
            interface: None,
            member: None,
        }
    }
    pub fn with_message_type(mut self, message_type: MessageType) -> Self {
        self.message_type = Some(message_type);
        self
    }
    pub fn with_reply_serial(mut self, reply_serial: u32) -> Self {
        self.reply_serial = Some(reply_serial);
        self
    }
    #[allow(unused)]
    pub fn with_sender(mut self, sender: String) -> Self {
        self.sender = Some(sender);
        self
    }
    pub fn with_path(mut self, path: String) -> Self {
        self.path = Some(path);
        self
    }
    pub fn with_interface(mut self, interface: String) -> Self {
        self.interface = Some(interface);
        self
    }
    pub fn with_member(mut self, member: String) -> Self {
        self.member = Some(member);
        self
    }
}

pub struct Connection2 {
    pub conn: Connection,
    next_matcher_id: usize,
    matchers: HashMap<usize, (usize, Matcher, Vec<Message>)>,
    unmatched_n: usize,
}
impl Connection2 {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn,
            next_matcher_id: 0,
            matchers: HashMap::new(),
            unmatched_n: 0,
        }
    }
    fn get_matcher_id(&mut self) -> usize {
        let id = self.next_matcher_id;
        self.next_matcher_id += 1;
        id
    }
    pub fn send(&self, message: Message) -> Result<u32, ()> {
        self.conn.send(message)
    }
    pub fn register_matcher(&mut self, n: usize, matcher: Matcher) -> usize {
        let id = self.get_matcher_id();
        self.matchers.insert(id, (n, matcher, Vec::new()));
        self.unmatched_n += n;
        id
    }
    pub fn remove_matcher(&mut self, id: usize) -> Option<(Matcher, Vec<Message>)> {
        let matcher = self.matchers.remove(&id);
        if let Some((n, _, ref messages)) = matcher {
            self.unmatched_n -= n - messages.len();
        }
        matcher.map(|(_, matcher, messages)| (matcher, messages))
    }
    fn match_message(&mut self, message: Message) -> Option<usize> {
        for (id, (n, matcher, messages)) in self.matchers.iter_mut() {
            if messages.len() >= *n {
                continue;
            }
            if matcher
                .message_type
                .is_some_and(|v| v != message.msg_type())
            {
                continue;
            }
            if matcher
                .reply_serial
                .is_some_and(|v| Some(v) != message.get_reply_serial())
            {
                continue;
            }
            if matcher
                .sender
                .as_deref()
                .is_some_and(|v| Some(v) != message.sender().as_deref())
            {
                continue;
            }
            if matcher
                .path
                .as_deref()
                .is_some_and(|v| Some(v) != message.path().as_deref())
            {
                continue;
            }
            if matcher
                .interface
                .as_deref()
                .is_some_and(|v| Some(v) != message.interface().as_deref())
            {
                continue;
            }
            if matcher
                .member
                .as_deref()
                .is_some_and(|v| Some(v) != message.member().as_deref())
            {
                continue;
            }
            messages.push(message);
            self.unmatched_n -= 1;
            return Some(*id);
        }
        None
    }
    pub fn block_on_any_io(&mut self, timeout: Duration) -> Result<(), dbus::Error> {
        let msg = self.conn.channel().blocking_pop_message(timeout)?;
        if let Some(msg) = msg {
            self.match_message(msg);
        };
        Ok(())
    }
    // pub fn block_on_all_matchers(&mut self, timeout: Duration) -> Result<usize, dbus::Error> {
    //     let begin_time = Instant::now();
    //     while self.unmatched_n > 0 {
    //         let remaining_timeout = timeout.saturating_sub(Instant::now() - begin_time);
    //         if remaining_timeout == Duration::ZERO {
    //             break;
    //         }
    //         self.block_on_any_io(remaining_timeout)?;
    //     }
    //     Ok(self.unmatched_n)
    // }
    pub fn block_on_matcher_remove(
        &mut self,
        id: usize,
        timeout: Duration,
    ) -> Result<(Matcher, Vec<Message>), dbus::Error> {
        if !self.matchers.contains_key(&id) {
            panic!("Matcher ID not registered");
        }
        let begin_time = Instant::now();
        loop {
            if self
                .matchers
                .get(&id)
                .is_some_and(|(n, _, messages)| messages.len() >= *n)
            {
                return Ok(self.remove_matcher(id).unwrap());
            }
            let remaining_timeout = timeout.saturating_sub(Instant::now() - begin_time);
            if remaining_timeout == Duration::ZERO {
                return Ok(self.remove_matcher(id).unwrap());
            }
            self.block_on_any_io(remaining_timeout)?;
        }
    }
    pub fn block_on_once_matcher_remove(
        &mut self,
        id: usize,
        timeout: Duration,
    ) -> Result<(Matcher, Option<Message>), dbus::Error> {
        self.block_on_matcher_remove(id, timeout)
            .map(|(matcher, messages)| (matcher, messages.into_iter().next()))
    }
}
