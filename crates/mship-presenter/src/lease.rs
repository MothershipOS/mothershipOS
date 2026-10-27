//! sway's wp_drm_lease_device_v1, as a client: the display's connector when sway offers it, the lease.

use std::os::fd::AsFd;
use std::sync::Mutex;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, event_created_child};
use wayland_protocols::wp::drm_lease::v1::client::{wp_drm_lease_connector_v1 as conn, wp_drm_lease_device_v1 as dev, wp_drm_lease_request_v1 as req, wp_drm_lease_v1 as lease};

use crate::{State, log};

pub struct Lease {
    conn: Connection,
    qh: QueueHandle<State>,
    device: Option<dev::WpDrmLeaseDeviceV1>,
    /// the offered connector of our display
    connector: Option<conn::WpDrmLeaseConnectorV1>,
    pub lease: Option<lease::WpDrmLeaseV1>,
    requested: bool,
    /// sway went away
    pub lost: bool,
}

impl Lease {
    pub fn flush(&self) {
        let _ = self.conn.flush();
    }

    pub fn destroy(&mut self) {
        if let Some(l) = self.lease.take() {
            l.destroy();
        }
        self.requested = false;
    }
}

/// The session's sway (WAYLAND_DISPLAY / XDG_RUNTIME_DIR).
pub fn connect() -> Result<(Lease, EventQueue<State>), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    Ok((Lease { conn, qh, device: None, connector: None, lease: None, requested: false, lost: false }, queue))
}

/// Bind the lease device (one roundtrip), then follow sway's events in the loop.
pub fn start(st: &mut State, mut queue: EventQueue<State>) -> Result<(), String> {
    queue.roundtrip(st).map_err(|e| e.to_string())?;
    queue.roundtrip(st).map_err(|e| e.to_string())?;
    if st.lease.device.is_none() {
        return Err("sway offers no wp_drm_lease_device_v1".into());
    }
    let fd = st.lease.conn.as_fd().try_clone_to_owned().map_err(|e| e.to_string())?;
    st.lh
        .insert_source(Generic::new(fd, Interest::READ, Mode::Level), move |_, _, st: &mut State| {
            let ok = match queue.prepare_read() {
                Some(g) => g.read().is_ok(),
                None => true,
            };
            if !ok || queue.dispatch_pending(st).is_err() {
                st.lease.lost = true;
            }
            st.lease.flush();
            Ok(PostAction::Continue)
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn try_lease(st: &mut State) {
    let l = &mut st.lease;
    let (Some(d), Some(c)) = (&l.device, &l.connector) else { return };
    if l.lease.is_some() || l.requested {
        return;
    }
    let r = d.create_lease_request(&l.qh, ());
    r.request_connector(c);
    l.lease = Some(r.submit(&l.qh, ()));
    l.requested = true;
    log(format!("requesting lease on {}", st.output));
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, r: &wl_registry::WlRegistry, e: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, .. } = e
            && interface == "wp_drm_lease_device_v1"
            && st.lease.device.is_none()
        {
            st.lease.device = Some(r.bind(name, 1, qh, ()));
        }
    }
}

impl Dispatch<dev::WpDrmLeaseDeviceV1, ()> for State {
    fn event(_: &mut Self, _: &dev::WpDrmLeaseDeviceV1, e: dev::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // drm_fd: not ours to use (dropped = closed); connectors are followed on their own
        drop(e);
    }

    event_created_child!(State, dev::WpDrmLeaseDeviceV1, [
        dev::EVT_CONNECTOR_OPCODE => (conn::WpDrmLeaseConnectorV1, Mutex::new(String::new())),
    ]);
}

impl Dispatch<conn::WpDrmLeaseConnectorV1, Mutex<String>> for State {
    fn event(st: &mut Self, c: &conn::WpDrmLeaseConnectorV1, e: conn::Event, name: &Mutex<String>, _: &Connection, _: &QueueHandle<Self>) {
        match e {
            conn::Event::Name { name: n } => *name.lock().unwrap() = n,
            conn::Event::Done if *name.lock().unwrap() == st.output => {
                log(format!("sway offers {}", st.output));
                st.lease.connector = Some(c.clone());
                try_lease(st);
            }
            conn::Event::Withdrawn => {
                if st.lease.connector.as_ref() == Some(c) {
                    log(format!("{} offer withdrawn", st.output));
                    st.lease.connector = None;
                }
                c.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<req::WpDrmLeaseRequestV1, ()> for State {
    fn event(_: &mut Self, _: &req::WpDrmLeaseRequestV1, _: req::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<lease::WpDrmLeaseV1, ()> for State {
    fn event(st: &mut Self, _: &lease::WpDrmLeaseV1, e: lease::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match e {
            lease::Event::LeaseFd { leased_fd } => st.lease_granted(leased_fd),
            lease::Event::Finished => st.lease_finished(),
            _ => {}
        }
    }
}
