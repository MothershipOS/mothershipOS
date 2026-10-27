//! A blocking mothershipd client (the `mship` command; the shell runs one on a thread).

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::api::{Event, Message, Op, Request, SOCKET, Topic};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("mothershipd is not running ({0})")]
    Connect(std::io::Error),
    #[error("mothershipd: {0}")]
    Io(#[from] std::io::Error),
    #[error("mothershipd sent something unreadable: {0}")]
    Proto(#[from] serde_json::Error),
    #[error("{0}")]
    Daemon(String),
}

pub struct Client {
    w: UnixStream,
    r: BufReader<UnixStream>,
    next: u64,
    /// events that arrived while waiting for a reply
    events: VecDeque<Event>,
}

impl Client {
    pub fn connect() -> Result<Self, ClientError> {
        Self::connect_to(SOCKET)
    }

    pub fn connect_to(path: &str) -> Result<Self, ClientError> {
        let w = UnixStream::connect(path).map_err(ClientError::Connect)?;
        let r = BufReader::new(w.try_clone()?);
        Ok(Self { w, r, next: 0, events: VecDeque::new() })
    }

    /// Replies take at most this long (None: wait; vm.stop and wifi.connect take a while).
    pub fn set_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        self.w.set_read_timeout(t)
    }

    fn line(&mut self) -> Result<Message, ClientError> {
        let mut s = String::new();
        if self.r.read_line(&mut s)? == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        Ok(serde_json::from_str(&s)?)
    }

    /// One request, its result.
    pub fn call(&mut self, op: Op) -> Result<Value, ClientError> {
        self.next += 1;
        let id = self.next;
        let mut b = serde_json::to_vec(&Request { id, op })?;
        b.push(b'\n');
        self.w.write_all(&b)?;
        loop {
            match self.line()? {
                Message::Event(e) => self.events.push_back(e),
                Message::Reply(r) if r.id == Some(id) => {
                    return if r.ok { Ok(r.result.unwrap_or(Value::Null)) } else { Err(ClientError::Daemon(r.error.unwrap_or_default())) };
                }
                Message::Reply(_) => {}
            }
        }
    }

    /// A request whose result has a known type.
    pub fn get<T: DeserializeOwned>(&mut self, op: Op) -> Result<T, ClientError> {
        Ok(serde_json::from_value(self.call(op)?)?)
    }

    pub fn subscribe(&mut self, topics: &[Topic]) -> Result<(), ClientError> {
        self.call(Op::Subscribe { topics: topics.to_vec() }).map(|_| ())
    }

    /// The next event (blocks).
    pub fn event(&mut self) -> Result<Event, ClientError> {
        if let Some(e) = self.events.pop_front() {
            return Ok(e);
        }
        loop {
            if let Message::Event(e) = self.line()? {
                return Ok(e);
            }
        }
    }
}

/// One request on a fresh connection.
pub fn call(op: Op) -> Result<Value, ClientError> {
    Client::connect()?.call(op)
}
