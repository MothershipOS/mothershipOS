//! mship-shell: the mothershipOS shell, the host's touch UI (GTK 4 + libadwaita); runs as user mship in the sway
//! session.
//!
//! Home: host status, virtual machines (start / show / sleep / stop), terminal, settings, power off / restart.
//! Resources: live CPU (per core), memory, GPU, battery power, network, storage, per-VM and top processes.
//! Settings: battery, Wi-Fi, Bluetooth, SSH, CPU modes, region & time, display, hardware owners, VM memory,
//! about, the raw power votes. Everything goes through mothershipd ([`broker`]); the on-screen keyboard (wvkbd)
//! shows while a text field or the terminal ([`terminal`]) has focus.

mod broker;
mod settings;
mod system;
mod terminal;
mod ui;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};
use mship_core::api::{HandoverState, Op, PowerState, ShowResult, Topic, VmInfo, VmList, VoteState};
use mship_core::status::{BatteryStatus, HostStatus, RegionStatus};
use serde::de::DeserializeOwned;
use serde_json::Value;

use broker::Broker;
use ui::{Rows, button, esc, group, mhz, page, row};

pub const VMS: &str = "/var/lib/mothership/vms";
const OSK: &str = "wvkbd-mobintl";
const CSS: &str = "
.big-title { font-size: 20pt; font-weight: 800; }
.clock { font-size: 34pt; font-weight: 300; }
.dim { opacity: 0.6; }
.vm-running { color: @success_color; }
";

/// Hardware that can be assigned to a VM (mothershipd handover.assign).
pub const HW: &[(&str, &str, &str)] = &[
    ("display", "Display & touchscreen", "video-display-symbolic"),
    ("buttons", "Volume buttons", "audio-volume-high-symbolic"),
    ("wifi-2g", "Wi-Fi 2.4 GHz", "network-wireless-symbolic"),
    ("wifi-5g", "Wi-Fi 5 GHz", "network-wireless-symbolic"),
    ("internet", "Shared internet", "network-transmit-receive-symbolic"),
    ("bluetooth", "Bluetooth", "bluetooth-active-symbolic"),
    ("usb", "USB devices", "media-removable-symbolic"),
    ("display-ext", "External display (USB-C)", "video-joined-displays-symbolic"),
    ("cellular", "Mobile network (SIM, calls, data)", "network-cellular-symbolic"),
    ("cameras", "Cameras", "camera-photo-symbolic"),
];
/// lent while the VM is shown; the rest while it runs
pub const HW_SCREEN: &[&str] = &["display", "buttons"];

pub const PROFILES: &[&str] = &["cool", "balanced", "performance"];

pub fn profile_name(p: &str) -> String {
    match p {
        "cool" => "Cool",
        "balanced" => "Balanced",
        "performance" => "Performance",
        "custom" => "Custom",
        other => other,
    }
    .into()
}

pub fn vm_conf(d: &str) -> Value {
    std::fs::read_to_string(format!("{VMS}/{d}/vm.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn vm_name(d: &str) -> String {
    vm_conf(d)["name"].as_str().map(String::from).unwrap_or_else(|| {
        match d {
            "ubuntu-touch-2004" => "Ubuntu Touch 20.04",
            "ubuntu-touch-2404" => "Ubuntu Touch 24.04",
            "ubuntu-touch" => "Ubuntu Touch 26.04 (dev)",
            d => d,
        }
        .into()
    })
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

struct Home {
    clock: gtk::Label,
    date: gtk::Label,
    batt: adw::ActionRow,
    wifi: adw::ActionRow,
    cpu: adw::ActionRow,
    ssh: adw::ActionRow,
    res: adw::ActionRow,
    vms: Rows,
}

pub struct Shell {
    win: adw::ApplicationWindow,
    nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    pub broker: Broker,
    pub status: RefCell<Option<HostStatus>>,
    clock24: Cell<bool>,
    batt_short: RefCell<String>,
    home: RefCell<Option<Home>>,
    home_cpu: Cell<Option<(u64, u64)>>,
}

pub type S = Rc<Shell>;

impl Shell {
    pub fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::builder().title(esc(msg)).timeout(4).build());
    }

    /// A mothershipd request; errors become a toast.
    pub fn ask(self: &Rc<Self>, op: Op, done: impl FnOnce(Value) + 'static) {
        let me = Rc::downgrade(self);
        self.broker.request(op, move |r| match r {
            Ok(v) => done(v),
            Err(e) => {
                if let Some(s) = me.upgrade() {
                    s.toast(&e);
                }
            }
        });
    }

    /// A typed request; errors become a toast.
    pub fn ask_t<T: DeserializeOwned + 'static>(self: &Rc<Self>, op: Op, done: impl FnOnce(T) + 'static) {
        let me = Rc::downgrade(self);
        self.broker.get(op, move |r: Result<T, String>| match r {
            Ok(v) => done(v),
            Err(e) => {
                if let Some(s) = me.upgrade() {
                    s.toast(&e);
                }
            }
        });
    }

    pub fn push(&self, p: &adw::NavigationPage) {
        self.nav.push(p);
    }

    pub fn pop(&self) {
        self.nav.pop();
    }

    pub fn window(&self) -> &adw::ApplicationWindow {
        &self.win
    }

    /// f when the page leaves the navigation stack.
    pub fn on_gone(&self, p: &adw::NavigationPage, f: impl Fn() + 'static) {
        let p = p.clone();
        self.nav.connect_popped(move |_, popped| {
            if *popped == p {
                f();
            }
        });
    }

    /// f on every change of these mothershipd topics while the page is on the stack.
    pub fn follow(self: &Rc<Self>, p: &adw::NavigationPage, topics: &[Topic], f: impl Fn() + 'static) {
        let f = Rc::new(f);
        let ls: Vec<broker::Listening> = topics
            .iter()
            .map(|t| {
                let f = f.clone();
                self.broker.listen(*t, move |_| f())
            })
            .collect();
        self.on_gone(p, move || ls.iter().for_each(|l| l.stop()));
    }

    /// f every `secs` while the page is on the stack.
    pub fn live(&self, p: &adw::NavigationPage, secs: u32, f: impl Fn() + 'static) {
        let alive = Rc::new(Cell::new(true));
        let a = alive.clone();
        glib::timeout_add_seconds_local(secs, move || {
            if !a.get() {
                return glib::ControlFlow::Break;
            }
            f();
            glib::ControlFlow::Continue
        });
        self.on_gone(p, move || alive.set(false));
    }

    pub fn vm_list(&self) -> Vec<VmInfo> {
        self.broker.state_as::<VmList>(Topic::Vms).vms
    }

    pub fn vm_info(&self, d: &str) -> Option<VmInfo> {
        self.vm_list().into_iter().find(|v| v.vm == d)
    }

    pub fn handover(&self) -> HandoverState {
        self.broker.state_as(Topic::Handover)
    }

    // ------------------------------------------------ home
    fn home_page(self: &Rc<Self>) -> adw::NavigationPage {
        let top = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).margin_top(18).margin_bottom(6).build();
        let clock = gtk::Label::builder().label(ui::local_time("%H:%M")).css_classes(["clock"]).build();
        let date = gtk::Label::builder().label("").css_classes(["dim"]).build();
        top.append(&clock);
        top.append(&date);

        let g_status = group("Host", "");
        let me = Rc::downgrade(self);
        let nav_to = move |f: fn(&S) -> adw::NavigationPage| {
            let me = me.clone();
            move |_: &adw::ActionRow| {
                if let Some(s) = me.upgrade() {
                    s.push(&f(&s));
                }
            }
        };
        let batt = row("Battery", "…", Some("battery-good-symbolic"), true);
        batt.connect_activated(nav_to(settings::battery_page));
        let wifi = row("Wi-Fi", "…", Some("network-wireless-symbolic"), true);
        wifi.connect_activated(nav_to(settings::wifi_page));
        let cpu = row("CPU", "…", Some("power-profile-performance-symbolic"), true);
        cpu.connect_activated(nav_to(settings::cpu_page));
        let ssh = row("SSH", "…", Some("network-server-symbolic"), true);
        ssh.connect_activated(nav_to(settings::ssh_page));
        let res = row("Resources", "…", Some("gnome-power-manager-symbolic"), true);
        res.connect_activated(nav_to(system::resources_page));
        for r in [&batt, &wifi, &cpu, &res, &ssh] {
            g_status.add(r);
        }

        let vms = Rows::new(group("Virtual machines", "The assist key or holding both volume buttons returns to mothershipOS"));

        let g_apps = group("Apps", "");
        let term = row("Terminal", "host shell (foot)", Some("utilities-terminal-symbolic"), true);
        term.connect_activated(|_| terminal::open());
        let sett = row("Settings", "Wi-Fi, SSH, CPU, display, hardware", Some("emblem-system-symbolic"), true);
        sett.connect_activated(nav_to(settings::settings_page));
        g_apps.add(&term);
        g_apps.add(&sett);

        let g_power = group("", "");
        let r_power = row("Power", "running VMs are shut down first", Some("system-shutdown-symbolic"), false);
        let me = Rc::downgrade(self);
        r_power.add_suffix(&button("Restart", None, move || {
            if let Some(s) = me.upgrade() {
                s.power_dialog(true);
            }
        }));
        let me = Rc::downgrade(self);
        r_power.add_suffix(&button("Power off", Some("destructive-action"), move || {
            if let Some(s) = me.upgrade() {
                s.power_dialog(false);
            }
        }));
        g_power.add(&r_power);

        let p = page("mothershipOS", &[&g_status, &vms.group, &g_apps, &g_power], None);
        p.toolbar.set_reveal_top_bars(false);
        let bx = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        bx.append(&top);
        p.prefs.set_vexpand(true);
        p.toolbar.set_content(None::<&gtk::Widget>);
        bx.append(&p.prefs);
        p.toolbar.set_content(Some(&bx));
        *self.home.borrow_mut() = Some(Home { clock, date, batt, wifi, cpu, ssh, res, vms });
        p.page
    }

    fn power_dialog(self: &Rc<Self>, reboot: bool) {
        let vms: Vec<String> = self.vm_list().into_iter().filter(|v| v.running).map(|v| v.name).collect();
        let verb = if reboot { "Restart" } else { "Power off" };
        let body = if vms.is_empty() { "No VM is running.".to_string() } else { format!("These VMs are shut down first: {}.", vms.join(", ")) };
        let dlg = adw::AlertDialog::new(Some(&format!("{verb} the phone?")), Some(&esc(&body)));
        dlg.add_response("cancel", "Cancel");
        dlg.add_response("go", verb);
        dlg.set_response_appearance("go", adw::ResponseAppearance::Destructive);
        let me = Rc::downgrade(self);
        dlg.connect_response(None, move |_, resp| {
            let Some(s) = me.upgrade() else { return };
            if resp != "go" {
                return;
            }
            s.toast(if vms.is_empty() { verb } else { "Shutting down the VMs…" });
            s.ask(if reboot { Op::Reboot } else { Op::PowerOff }, |_| {});
        });
        dlg.present(Some(&self.win));
    }

    fn tick_clock(&self) {
        let h = self.home.borrow();
        let Some(h) = h.as_ref() else { return };
        h.clock.set_label(&ui::local_time(if self.clock24.get() { "%H:%M" } else { "%-I:%M %p" }));
        let bs = self.batt_short.borrow();
        h.date.set_label(&format!("{}{}", ui::local_time("%A %-d %B"), if bs.is_empty() { String::new() } else { format!("  ·  {bs}") }));
    }

    /// The home screen's CPU and memory line.
    fn tick_res(&self) {
        let cpu = system::cpu_times().first().copied();
        let prev = self.home_cpu.replace(cpu);
        let m = system::meminfo();
        let (total, avail) = (m.get("MemTotal").copied().unwrap_or(0) as f64, m.get("MemAvailable").copied().unwrap_or(0) as f64);
        let mem = format!("memory {:.1} of {:.1} GB", (total - avail) / 1048576.0, total / 1048576.0);
        if let (Some(c), Some(p), Some(h)) = (cpu, prev, self.home.borrow().as_ref())
            && c.1 > p.1
        {
            h.res.set_subtitle(&format!("CPU {:.0} % · {mem}", 100.0 * (c.0 - p.0) as f64 / (c.1 - p.1) as f64));
        }
    }

    fn on_status(&self, s: HostStatus) {
        {
            let h = self.home.borrow();
            let Some(h) = h.as_ref() else { return };
            let b = &s.battery;
            h.batt.set_subtitle(&esc(&batt_text(b)));
            *self.batt_short.borrow_mut() = if b.present {
                format!("{}%{}", b.capacity.unwrap_or(0), if b.status.as_deref() == Some("Charging") { " ⚡" } else { "" })
            } else {
                String::new()
            };
            let w = &s.wifi;
            if w.owner != "host" {
                h.wifi.set_subtitle(&esc(&format!("lent to {}", if w.owner_name.is_empty() { &w.owner } else { &w.owner_name })));
            } else if w.state == "connected" {
                h.wifi.set_subtitle(&esc(&format!("{} · {} · {} dBm", w.ssid.as_deref().unwrap_or(""), w.ip.as_deref().unwrap_or(""), w.signal.unwrap_or(0))));
            } else {
                h.wifi.set_subtitle(if w.radio { &w.state } else { "off" });
            }
            let c = &s.cpu;
            let fr: Vec<String> = c.policies.iter().map(|p| mhz(p.cur)).collect();
            let hot = c.hottest.map_or(String::new(), |t| format!(" · {t:.0} °C"));
            let mode = profile_name(c.profile.as_deref().unwrap_or("custom"));
            h.cpu.set_subtitle(&format!("{mode}{} · {}{hot}", if c.throttled { " (throttling)" } else { "" }, fr.join(" / ")));
            let ss = &s.ssh;
            h.ssh.set_subtitle(&if ss.enabled && ss.active {
                ss.addresses.iter().map(|a| a.split_whitespace().next().unwrap_or("")).collect::<Vec<_>>().join(", ")
            } else {
                "off".into()
            });
        }
        *self.status.borrow_mut() = Some(s);
    }

    fn refresh_vms(self: &Rc<Self>) {
        let h = self.home.borrow();
        let Some(h) = h.as_ref() else { return };
        h.vms.clear();
        let own = self.handover().holders;
        let power: PowerState = self.broker.state_as(Topic::Power);
        if self.broker.state(Topic::Vms).is_none() && !self.broker.connected() {
            h.vms.add(&row("mothershipd is not running", "VMs cannot be started or shown", None, false));
            return;
        }
        let mut vms = self.vm_list();
        vms.sort_by_key(|v| (!v.running, !v.manifest, v.vm.clone()));
        for v in vms {
            let d = v.vm.clone();
            let path = format!("{VMS}/{d}");
            let asleep = v.running && power.participants.get(&d).is_some_and(|e| e.state == VoteState::Suspended);
            let sub = if let Some(b) = &v.busy {
                format!("{b}…")
            } else if asleep {
                "asleep · a key or the mouse wakes it".into()
            } else if v.running {
                if own.get("display") == Some(&path) { "running · has the screen".into() } else { "running".into() }
            } else {
                "stopped".into()
            };
            let r = row(&v.name, &sub, Some(if v.manifest { "phone-symbolic" } else { "computer-symbolic" }), false);
            let act = |label: &str, style: Option<&str>, f: fn(&S, &str)| {
                let (me, d) = (Rc::downgrade(self), d.clone());
                button(label, style, move || {
                    if let Some(s) = me.upgrade() {
                        f(&s, &d);
                    }
                })
            };
            if asleep {
                r.add_suffix(&act("Wake", Some("suggested-action"), |s, d| s.vm_power(d, true)));
                r.add_suffix(&act("Stop", Some("destructive-action"), Shell::stop_vm));
            } else if v.running {
                r.add_suffix(&act("Show", Some("suggested-action"), Shell::show_vm));
                r.add_suffix(&act("Stop", Some("destructive-action"), Shell::stop_vm));
            } else if v.busy.is_none() {
                r.add_suffix(&act("Start", None, Shell::start_vm)); // VMs run side by side
            }
            if v.manifest {
                let g = gtk::Button::builder().icon_name("emblem-system-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).build();
                let (me, d) = (Rc::downgrade(self), d.clone());
                g.connect_clicked(move |_| {
                    if let Some(s) = me.upgrade() {
                        s.push(&system::vm_page(&s, &d));
                    }
                });
                r.add_suffix(&g);
            }
            h.vms.add(&r);
        }
    }

    fn start_vm(self: &S, d: &str) {
        self.toast(&format!("Starting {}…", vm_name(d)));
        self.ask(Op::VmStart { vm: d.into() }, |_| {});
    }

    fn show_vm(self: &S, d: &str) {
        let me = Rc::downgrade(self);
        self.ask_t(Op::VmShow { vm: d.into() }, move |r: ShowResult| {
            if let (Some(m), Some(s)) = (r.message, me.upgrade()) {
                s.toast(&m);
            }
        });
    }

    fn stop_vm(self: &S, d: &str) {
        self.toast(&format!("Stopping {}…", vm_name(d)));
        self.ask(Op::VmStop { vm: d.into() }, |_| {});
    }

    pub fn vm_power(self: &S, d: &str, wake: bool) {
        self.toast(&if wake { format!("Waking {}…", vm_name(d)) } else { format!("Putting {} to sleep…", vm_name(d)) });
        let op = if wake { Op::VmWake { vm: d.into() } } else { Op::VmSleep { vm: d.into() } };
        self.ask(op, |_| {});
    }

    /// The keyboard for the shell's own text fields (while the shell is the focused window: a focused terminal has
    /// its own, [`terminal`]).
    fn on_focus(&self) {
        if !self.win.is_active() {
            return;
        }
        let w = gtk::prelude::GtkWindowExt::focus(&self.win);
        let editing = w.is_some_and(|w| w.is::<gtk::Text>() || w.is::<gtk::TextView>());
        ui::signal_named(OSK, if editing { libc::SIGUSR2 } else { libc::SIGUSR1 });
    }
}

pub fn batt_text(b: &BatteryStatus) -> String {
    if !b.present {
        return "no battery driver".into();
    }
    let cap = b.capacity.unwrap_or(0);
    let online = b.charger.as_ref().is_some_and(|c| c.online);
    let txt = match b.status.as_deref() {
        Some("Charging") if b.current < 0.0 => format!("{cap}% · plugged in, charger too weak ({:.0} mA)", b.current),
        Some("Charging") => format!("{cap}% · charging {:.0} mA", b.current),
        Some("Full") => format!("{cap}% · full"),
        _ if online => format!("{cap}% · plugged in, not charging"),
        _ => format!("{cap}% · {:.1} W", b.power.abs()),
    };
    format!("{txt} · {:.0} °C", b.temp)
}

fn activate(app: &adw::Application) {
    if let Some(w) = app.windows().first() {
        w.present();
        return;
    }
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let prov = gtk::CssProvider::new();
    prov.load_from_string(CSS);
    if let Some(d) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&d, &prov, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    let win = adw::ApplicationWindow::builder().application(app).title("mothershipOS").default_width(540).default_height(1200).build();
    let toasts = adw::ToastOverlay::new();
    let nav = adw::NavigationView::new();
    toasts.set_child(Some(&nav));
    win.set_content(Some(&toasts));
    let sh = Rc::new(Shell {
        win: win.clone(),
        nav,
        toasts,
        broker: Broker::new(&[Topic::Power, Topic::Vms, Topic::Handover, Topic::Host]),
        status: RefCell::default(),
        clock24: Cell::new(true),
        batt_short: RefCell::default(),
        home: RefCell::default(),
        home_cpu: Cell::new(None),
    });
    let me = Rc::downgrade(&sh);
    win.connect_focus_widget_notify(move |_| {
        if let Some(s) = me.upgrade() {
            s.on_focus();
        }
    });
    // back from a terminal (or a VM window): the keyboard as the shell's focus wants it
    let me = Rc::downgrade(&sh);
    win.connect_is_active_notify(move |_| {
        if let Some(s) = me.upgrade() {
            s.on_focus();
        }
    });
    terminal::watch(OSK);
    // a VM started / stopped / asleep / got the screen: at once
    for t in [Topic::Power, Topic::Vms, Topic::Handover] {
        let me = Rc::downgrade(&sh);
        std::mem::forget(sh.broker.listen(t, move |_| {
            if let Some(s) = me.upgrade() {
                s.refresh_vms();
            }
        }));
    }
    let me = Rc::downgrade(&sh);
    std::mem::forget(sh.broker.listen(Topic::Host, move |v| {
        if let (Some(s), Some(st)) = (me.upgrade(), v.and_then(|v| serde_json::from_value::<HostStatus>(v.clone()).ok())) {
            s.on_status(st);
        }
    }));
    let home = sh.home_page();
    sh.nav.add(&home);
    win.present();
    sh.tick_res();
    sh.refresh_vms();
    let me = Rc::downgrade(&sh);
    glib::timeout_add_seconds_local(1, move || {
        // the region status arrives once the daemon is connected
        let Some(s) = me.upgrade() else { return glib::ControlFlow::Break };
        if !s.broker.connected() {
            return glib::ControlFlow::Continue;
        }
        let me = Rc::downgrade(&s);
        s.broker.get(Op::RegionStatus, move |r: Result<RegionStatus, String>| {
            if let (Ok(r), Some(s)) = (r, me.upgrade()) {
                s.clock24.set(r.conf.clock24);
            }
        });
        glib::ControlFlow::Break
    });
    let me = Rc::downgrade(&sh);
    glib::timeout_add_seconds_local(4, move || {
        let Some(s) = me.upgrade() else { return glib::ControlFlow::Break };
        s.tick_res();
        glib::ControlFlow::Continue
    });
    let me = Rc::downgrade(&sh);
    glib::timeout_add_seconds_local(1, move || {
        let Some(s) = me.upgrade() else { return glib::ControlFlow::Break };
        s.tick_clock();
        glib::ControlFlow::Continue
    });
    // the shell lives as long as its window
    unsafe { win.set_data("shell", sh) };
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id("os.mothership.Shell").build();
    app.connect_activate(activate);
    app.run()
}
