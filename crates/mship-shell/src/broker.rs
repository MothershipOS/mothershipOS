//! mothershipd on the GLib main loop: one connection, requests with callbacks, subscribed topics kept current
//! (pushed by the daemon on every change) and passed to listeners; reconnects when the daemon restarts (the state
//! is then empty and listeners get None).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio::prelude::*;
use gtk::{gio, glib};
use mship_core::api::{Message, Op, Request, SOCKET, Topic};
use serde::de::DeserializeOwned;
use serde_json::Value;

type Reply = Box<dyn FnOnce(Result<Value, String>)>;
type Listener = Rc<dyn Fn(Option<&Value>)>;

#[derive(Default)]
struct Inner {
    topics: RefCell<Vec<Topic>>,
    state: RefCell<HashMap<Topic, Value>>,
    listeners: RefCell<HashMap<Topic, Vec<(u64, Listener)>>>,
    out: RefCell<Option<gio::OutputStream>>,
    pending: RefCell<HashMap<u64, Reply>>,
    next: Cell<u64>,
}

#[derive(Clone)]
pub struct Broker(Rc<Inner>);

/// Stops a listener when dropped... or when called.
pub struct Listening(Broker, Topic, u64);

impl Listening {
    pub fn stop(&self) {
        if let Some(v) = self.0.0.listeners.borrow_mut().get_mut(&self.1) {
            v.retain(|(id, _)| *id != self.2);
        }
    }
}

impl Broker {
    pub fn new(topics: &[Topic]) -> Self {
        let b = Broker(Rc::new(Inner { topics: RefCell::new(topics.to_vec()), ..Default::default() }));
        let me = b.clone();
        glib::spawn_future_local(async move { me.run().await });
        b
    }

    async fn run(self) {
        loop {
            let addr = gio::UnixSocketAddress::new(std::path::Path::new(SOCKET));
            if let Ok(conn) = gio::SocketClient::new().connect_future(&addr).await {
                let inp = gio::DataInputStream::new(&conn.input_stream());
                *self.0.out.borrow_mut() = Some(conn.output_stream());
                let topics = self.0.topics.borrow().clone();
                self.request(Op::Subscribe { topics }, |_| {});
                while let Ok(Some(line)) = inp.read_line_utf8_future(glib::Priority::DEFAULT).await {
                    self.on_line(&line);
                }
                drop(conn);
                self.lost();
            }
            glib::timeout_future_seconds(2).await;
        }
    }

    fn lost(&self) {
        *self.0.out.borrow_mut() = None;
        self.0.state.borrow_mut().clear();
        let topics = self.0.topics.borrow().clone();
        for t in topics {
            self.emit(t);
        }
        let pending: Vec<Reply> = self.0.pending.borrow_mut().drain().map(|(_, r)| r).collect();
        for r in pending {
            r(Err("mothershipd restarted".into()));
        }
    }

    fn on_line(&self, line: &str) {
        match serde_json::from_str::<Message>(line) {
            Ok(Message::Event(e)) => {
                self.0.state.borrow_mut().insert(e.event, e.data);
                self.emit(e.event);
            }
            Ok(Message::Reply(r)) => {
                let cb = r.id.and_then(|id| self.0.pending.borrow_mut().remove(&id));
                if let Some(cb) = cb {
                    cb(if r.ok { Ok(r.result.unwrap_or(Value::Null)) } else { Err(r.error.unwrap_or_else(|| "failed".into())) });
                }
            }
            Err(_) => {}
        }
    }

    fn emit(&self, t: Topic) {
        let ls: Vec<Listener> = self.0.listeners.borrow().get(&t).map(|v| v.iter().map(|(_, f)| f.clone()).collect()).unwrap_or_default();
        let st = self.0.state.borrow().get(&t).cloned();
        for f in ls {
            f(st.as_ref());
        }
    }

    pub fn connected(&self) -> bool {
        self.0.out.borrow().is_some()
    }

    /// A topic's current data.
    pub fn state(&self, t: Topic) -> Option<Value> {
        self.0.state.borrow().get(&t).cloned()
    }

    pub fn state_as<T: DeserializeOwned + Default>(&self, t: Topic) -> T {
        self.state(t).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    /// Add/remove a topic of this connection (e.g. while a page that needs it is open).
    pub fn subscribe(&self, t: Topic, on: bool) {
        {
            let mut topics = self.0.topics.borrow_mut();
            if on && !topics.contains(&t) {
                topics.push(t);
            } else if !on {
                topics.retain(|x| *x != t);
                self.0.state.borrow_mut().remove(&t);
            }
        }
        let op = if on { Op::Subscribe { topics: vec![t] } } else { Op::Unsubscribe { topics: vec![t] } };
        self.request(op, |_| {});
    }

    /// f(state) on every change of the topic, until the returned handle is stopped.
    pub fn listen(&self, t: Topic, f: impl Fn(Option<&Value>) + 'static) -> Listening {
        let id = self.0.next.get() + 1;
        self.0.next.set(id);
        self.0.listeners.borrow_mut().entry(t).or_default().push((id, Rc::new(f)));
        Listening(self.clone(), t, id)
    }

    pub fn request(&self, op: Op, cb: impl FnOnce(Result<Value, String>) + 'static) {
        let out = self.0.out.borrow().clone();
        let Some(out) = out else { return cb(Err("mothershipd is not running".into())) };
        let id = self.0.next.get() + 1;
        self.0.next.set(id);
        let mut line = serde_json::to_vec(&Request { id, op }).unwrap_or_default();
        line.push(b'\n');
        self.0.pending.borrow_mut().insert(id, Box::new(cb));
        if let Err(e) = out.write_all(&line, gio::Cancellable::NONE)
            && let Some(cb) = self.0.pending.borrow_mut().remove(&id)
        {
            cb(Err(e.to_string()));
        }
    }

    /// A request whose result has a known type.
    pub fn get<T: DeserializeOwned + 'static>(&self, op: Op, cb: impl FnOnce(Result<T, String>) + 'static) {
        self.request(op, move |r| cb(r.and_then(|v| serde_json::from_value(v).map_err(|e| format!("unexpected answer: {e}")))));
    }
}
