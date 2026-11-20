//! mship-shell: the mothershipOS shell, the host's touch UI (GTK 4 + libadwaita); runs as user mship in the sway
//! session.
//!
//! Two places: Home is the VMs (start / show / stop, a search of the VM store, the store; a VM's page [`vm`] has
//! everything about it: hardware, performance, memory, update), the Phone page is the host (performance, Wi-Fi,
//! Bluetooth, SSH, battery, display, region & time, who owns which hardware, terminal, about, power off). Everything
//! goes through mothershipd ([`broker`]); the on-screen keyboard (wvkbd) shows while a text field or the terminal
//! ([`terminal`]) has focus.

mod broker;
mod settings;
mod store;
mod system;
mod terminal;
mod ui;
mod vm;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};
use mship_core::api::{HandleList, HandoverState, Op, PowerState, ShowResult, StoreStatus, Topic, VmInfo, VmList, VoteState};
use mship_core::status::{BatteryStatus, HostStatus, RegionStatus};
use serde::de::DeserializeOwned;
use serde_json::Value;

use broker::Broker;
use ui::{Rows, button, esc, group, page, row};

pub const VMS: &str = "/var/lib/mothership/vms";
const OSK: &str = "wvkbd-mobintl";
const CSS: &str = "
.big-title { font-size: 20pt; font-weight: 800; }
.clock { font-size: 34pt; font-weight: 300; }
.dim { opacity: 0.6; }
.vm-running { color: @success_color; }
";


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
    phone: adw::ActionRow,
    shop: adw::ActionRow,
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

    /// The hardware mothershipOS knows (the handle registry): what a VM's page › Hardware shows.
    pub fn handles(&self) -> HandleList {
        self.broker.state_as(Topic::Handles)
    }

    // ------------------------------------------------ home
    fn home_page(self: &Rc<Self>) -> adw::NavigationPage {
        let top = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).margin_top(18).margin_bottom(6).build();
        let clock = gtk::Label::builder().label(ui::local_time("%H:%M")).css_classes(["clock"]).build();
        let date = gtk::Label::builder().label("").css_classes(["dim"]).build();
        top.append(&clock);
        top.append(&date);

        // the phone: one line of its state, everything about it behind it
        let g_phone = group("", "");
        let phone = row("Phone", "…", Some("emblem-system-symbolic"), true);
        let me = Rc::downgrade(self);
        phone.connect_activated(move |_| {
            if let Some(s) = me.upgrade() {
                s.push(&settings::phone_page(&s));
            }
        });
        g_phone.add(&phone);

        // the VMs: a search of the store, the installed ones, the store
        let vms = Rows::new(group("Virtual machines", "Tap a VM for its hardware, performance and settings. The assist key or holding both volume buttons returns here."));
        let search = gtk::SearchEntry::builder().placeholder_text("Find VMs to install").hexpand(true).build();
        let me = Rc::downgrade(self);
        search.connect_activate(move |e| {
            if let Some(s) = me.upgrade() {
                s.push(&store::store_page_query(&s, &e.text()));
                e.set_text("");
            }
        });
        let g_search = group("", "");
        g_search.add(&search);
        let g_find = group("", "");
        let shop = row("VM store", "find, install and update VMs", Some("system-software-install-symbolic"), true);
        let me = Rc::downgrade(self);
        shop.connect_activated(move |_| {
            if let Some(s) = me.upgrade() {
                s.push(&store::store_page(&s));
            }
        });
        g_find.add(&shop);

        let p = page("mothershipOS", &[&g_phone, &vms.group, &g_search, &g_find], None);
        p.toolbar.set_reveal_top_bars(false);
        let bx = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        bx.append(&top);
        p.prefs.set_vexpand(true);
        p.toolbar.set_content(None::<&gtk::Widget>);
        bx.append(&p.prefs);
        p.toolbar.set_content(Some(&bx));
        // no focus on the search until it is tapped (else the keyboard covers the home screen)
        let win = self.win.clone();
        p.page.connect_shown(move |_| gtk::prelude::GtkWindowExt::set_focus(&win, None::<&gtk::Widget>));
        *self.home.borrow_mut() = Some(Home { clock, date, phone, shop, vms });
        p.page
    }

    pub fn power_dialog(self: &Rc<Self>, reboot: bool) {
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

    /// The home screen's store line: updates of the installed VMs (mothershipd checks the store).
    fn tick_store(self: &Rc<Self>) {
        let me = Rc::downgrade(self);
        self.broker.get(Op::StoreStatus, move |r: Result<StoreStatus, String>| {
            if let (Ok(st), Some(s)) = (r, me.upgrade())
                && let Some(h) = s.home.borrow().as_ref()
            {
                h.shop.set_subtitle(&esc(&store::summary(&st)));
            }
        });
    }

    fn tick_clock(&self) {
        let h = self.home.borrow();
        let Some(h) = h.as_ref() else { return };
        h.clock.set_label(&ui::local_time(if self.clock24.get() { "%H:%M" } else { "%-I:%M %p" }));
        let bs = self.batt_short.borrow();
        h.date.set_label(&format!("{}{}", ui::local_time("%A %-d %B"), if bs.is_empty() { String::new() } else { format!("  ·  {bs}") }));
    }

    fn on_status(&self, s: HostStatus) {
        {
            let h = self.home.borrow();
            let Some(h) = h.as_ref() else { return };
            let b = &s.battery;
            *self.batt_short.borrow_mut() = if b.present {
                format!("{}%{}", b.capacity.unwrap_or(0), if b.status.as_deref() == Some("Charging") { " ⚡" } else { "" })
            } else {
                String::new()
            };
            // battery · Wi-Fi · CPU mode
            let w = &s.wifi;
            let wifi = if w.owner != "host" {
                format!("Wi-Fi lent to {}", if w.owner_name.is_empty() { &w.owner } else { &w.owner_name })
            } else if w.state == "connected" {
                w.ssid.clone().unwrap_or_else(|| "Wi-Fi".into())
            } else {
                format!("Wi-Fi {}", if w.radio { w.state.as_str() } else { "off" })
            };
            let c = &s.cpu;
            let mode = profile_name(c.profile.as_deref().unwrap_or("custom"));
            let mut parts = vec![];
            if b.present {
                parts.push(format!("{}%", b.capacity.unwrap_or(0)));
            }
            parts.push(wifi);
            parts.push(format!("{mode}{}", if c.throttled { " (throttling)" } else { "" }));
            h.phone.set_subtitle(&esc(&parts.join(" · ")));
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
            if v.manifest {
                r.set_activatable(true);
                let (me, d) = (Rc::downgrade(self), d.clone());
                r.connect_activated(move |_| {
                    if let Some(s) = me.upgrade() {
                        s.push(&vm::vm_page(&s, &d));
                    }
                });
            }
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
            h.vms.add(&r);
        }
    }

    pub fn start_vm(self: &S, d: &str) {
        self.toast(&format!("Starting {}…", vm_name(d)));
        self.ask(Op::VmStart { vm: d.into() }, |_| {});
    }

    pub fn show_vm(self: &S, d: &str) {
        let me = Rc::downgrade(self);
        self.ask_t(Op::VmShow { vm: d.into() }, move |r: ShowResult| {
            if let (Some(m), Some(s)) = (r.message, me.upgrade()) {
                s.toast(&m);
            }
        });
    }

    pub fn stop_vm(self: &S, d: &str) {
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
        broker: Broker::new(&[Topic::Power, Topic::Vms, Topic::Handover, Topic::Host, Topic::Handles]),
        status: RefCell::default(),
        clock24: Cell::new(true),
        batt_short: RefCell::default(),
        home: RefCell::default(),
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
    // development: MSHIP_SHELL_PAGE=phone|performance|vm:<dir> opens that page (screenshots without touch)
    if let Ok(want) = std::env::var("MSHIP_SHELL_PAGE") {
        let me = Rc::downgrade(&sh);
        glib::timeout_add_seconds_local(2, move || {
            if let Some(s) = me.upgrade() {
                match want.split_once(':') {
                    Some(("vm", d)) => s.push(&vm::vm_page(&s, d)),
                    _ if want == "performance" => s.push(&system::resources_page(&s)),
                    _ => s.push(&settings::phone_page(&s)),
                }
            }
            glib::ControlFlow::Break
        });
    }
    sh.refresh_vms();
    let me = Rc::downgrade(&sh);
    glib::timeout_add_seconds_local(1, move || {
        // the region status arrives once the daemon is connected
        let Some(s) = me.upgrade() else { return glib::ControlFlow::Break };
        if !s.broker.connected() {
            return glib::ControlFlow::Continue;
        }
        s.tick_store();
        let me = Rc::downgrade(&s);
        s.broker.get(Op::RegionStatus, move |r: Result<RegionStatus, String>| {
            if let (Ok(r), Some(s)) = (r, me.upgrade()) {
                s.clock24.set(r.conf.clock24);
            }
        });
        glib::ControlFlow::Break
    });
    let me = Rc::downgrade(&sh);
    glib::timeout_add_seconds_local(60, move || {
        let Some(s) = me.upgrade() else { return glib::ControlFlow::Break };
        s.tick_store();
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
