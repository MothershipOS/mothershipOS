//! The daemon's live state per topic, and the connected clients it pushes changes to.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use mship_core::api::{Event, Topic};
use serde_json::Value;

pub struct Client {
    pub uid: u32,
    pub topics: Mutex<BTreeSet<Topic>>,
    writer: Mutex<UnixStream>,
}

impl Client {
    pub fn new(stream: UnixStream, uid: u32) -> Self {
        // a client that stops reading is dropped, not waited for
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
        Self { uid, topics: Mutex::new(BTreeSet::new()), writer: Mutex::new(stream) }
    }

    pub fn send(&self, msg: &impl serde::Serialize) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.writer.lock().unwrap().write_all(&line)
    }

    pub fn close(&self) {
        let _ = self.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Default)]
pub struct Hub {
    clients: Mutex<Vec<Arc<Client>>>,
    /// topic -> (its current data, the part that decides whether it changed)
    data: Mutex<BTreeMap<Topic, (Value, Value)>>,
}

impl Hub {
    pub fn add(&self, c: Arc<Client>) {
        self.clients.lock().unwrap().push(c);
    }

    pub fn drop_client(&self, c: &Arc<Client>) {
        self.clients.lock().unwrap().retain(|x| !Arc::ptr_eq(x, c));
        c.close();
    }

    pub fn get(&self, t: Topic) -> Value {
        self.data.lock().unwrap().get(&t).map(|(v, _)| v.clone()).unwrap_or(Value::Null)
    }

    /// New data for a topic: pushed to its subscribers when it changed.
    pub fn set(&self, t: Topic, v: Value) -> bool {
        let key = v.clone();
        self.set_keyed(t, v, key)
    }

    /// New data whose change is judged by `key` only (e.g. without timestamps).
    pub fn set_keyed(&self, t: Topic, v: Value, key: Value) -> bool {
        let changed = {
            let mut d = self.data.lock().unwrap();
            let changed = d.get(&t).is_none_or(|(_, k)| *k != key);
            d.insert(t, (v, key));
            changed
        };
        if changed {
            self.publish(t);
        }
        changed
    }

    /// Push a topic's current data to its subscribers.
    pub fn publish(&self, t: Topic) {
        let ev = Event { event: t, data: self.get(t) };
        let targets: Vec<Arc<Client>> =
            self.clients.lock().unwrap().iter().filter(|c| c.topics.lock().unwrap().contains(&t)).cloned().collect();
        for c in targets {
            if c.send(&ev).is_err() {
                self.drop_client(&c);
            }
        }
    }

    /// Does any client subscribe to this topic?
    pub fn wanted(&self, t: Topic) -> bool {
        self.clients.lock().unwrap().iter().any(|c| c.topics.lock().unwrap().contains(&t))
    }
}
