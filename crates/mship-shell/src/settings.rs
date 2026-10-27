//! Settings: the list, and the battery, Wi-Fi, Bluetooth, SSH, CPU, region & time and display pages.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use mship_core::api::{Op, Topic};
use mship_core::status::{BatteryStatus, BtFound, BtStatus, Country, CpuStatus, HostStatus, PowerStatus, RegionStatus, SshStatus, WifiNetwork, WifiStatus};

use crate::ui::{Rows, button, esc, group, mhz, page, row, strings};
use crate::{PROFILES, S, base, profile_name, system, vm_name};

/// Run `f` with the shell, if it is still there.
fn with(s: &S, f: impl Fn(&S) + 'static) -> impl Fn() + 'static {
    let me = Rc::downgrade(s);
    move || {
        if let Some(s) = me.upgrade() {
            f(&s);
        }
    }
}

type PageFn = fn(&S) -> adw::NavigationPage;

pub fn settings_page(s: &S) -> adw::NavigationPage {
    let g = group("", "");
    let items: &[(&str, &str, &str, PageFn)] = &[
        ("Battery", "charging, charge limit", "battery-good-symbolic", battery_page),
        ("Wi-Fi", "networks, ownership", "network-wireless-symbolic", wifi_page),
        ("Bluetooth", "devices, pairing", "bluetooth-active-symbolic", bt_page),
        ("SSH", "remote login", "network-server-symbolic", ssh_page),
        ("CPU & performance", "Cool / Balanced / Performance, clocks, temperatures", "power-profile-performance-symbolic", cpu_page),
        ("Resources", "live CPU, memory, GPU, power, network; per VM", "gnome-power-manager-symbolic", system::resources_page),
        ("Region & time", "country (Wi-Fi rules), time zone, clock", "preferences-system-time-symbolic", region_page),
        ("Display", "brightness, scale, screen & sleep", "video-display-symbolic", display_page),
        ("Hardware", "assign devices to VMs: display, buttons, Wi-Fi bands", "drive-multidisk-symbolic", system::hw_page),
        ("Virtual machines", "shared memory and CPUs; limits", "computer-symbolic", system::vm_mem_page),
        ("About", "system information", "help-about-symbolic", system::about_page),
        ("Power votes", "diagnostic: raw mship.power table", "utilities-terminal-symbolic", system::power_page),
    ];
    for (title, sub, icon, f) in items {
        let r = row(title, sub, Some(icon), true);
        let f = *f;
        let go = with(s, move |s| s.push(&f(s)));
        r.connect_activated(move |_| go());
        g.add(&r);
    }
    page("Settings", &[&g], None).page
}

/// A full-page searchable list; items = [(label, value)]. Used instead of popover drop-downs: a popover is
/// dismissed as soon as the on-screen keyboard (another Wayland client) is touched.
pub fn pick(s: &S, title: &str, items: Vec<(String, String)>, current: &str, cb: impl Fn(&S, String) + 'static) {
    let search = gtk::SearchEntry::builder().placeholder_text("Search").margin_start(12).margin_end(12).margin_top(6).margin_bottom(6).build();
    let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).margin_start(12).margin_end(12).margin_bottom(12).valign(gtk::Align::Start).build();
    let cb = Rc::new(cb);
    let mut rows = vec![];
    for (label, value) in items {
        let r = adw::ActionRow::builder().title(esc(&label)).activatable(true).build();
        if value == current {
            r.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
        }
        let (cb, me) = (cb.clone(), Rc::downgrade(s));
        r.connect_activated(move |_| {
            if let Some(s) = me.upgrade() {
                s.pop();
                cb(&s, value.clone());
            }
        });
        lb.append(&r);
        rows.push((label.to_lowercase(), r));
    }
    search.connect_search_changed(move |e| {
        let q = e.text().to_lowercase();
        for (l, r) in &rows {
            r.set_visible(l.contains(&q));
        }
    });
    let sw = gtk::ScrolledWindow::builder().vexpand(true).child(&lb).hscrollbar_policy(gtk::PolicyType::Never).build();
    let bx = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    bx.append(&search);
    bx.append(&sw);
    let tv = adw::ToolbarView::builder().content(&bx).build();
    tv.add_top_bar(&adw::HeaderBar::builder().decoration_layout("").build());
    s.push(&adw::NavigationPage::new(&tv, title));
}

// ------------------------------------------------------------------ Wi-Fi
struct Wifi {
    banner: adw::Banner,
    switch: adw::SwitchRow,
    cur: adw::ActionRow,
    disc: gtk::Button,
    nets: Rows,
    saved: Rows,
    updating: Cell<bool>,
}

fn wifi_reload(s: &S, w: &Rc<Wifi>) {
    let w = w.clone();
    let me = Rc::downgrade(s);
    s.ask_t(Op::WifiStatus, move |st: WifiStatus| {
        if let Some(s) = me.upgrade() {
            wifi_show(&s, &w, &st);
        }
    });
}

fn wifi_show(s: &S, w: &Rc<Wifi>, st: &WifiStatus) {
    w.updating.set(true);
    w.switch.set_active(st.radio);
    w.updating.set(false);
    let owned = st.owner == "host";
    w.banner.set_title(&esc(&format!("Wi-Fi is lent to {}. Take it back to change networks.", if st.owner_name.is_empty() { &st.owner } else { &st.owner_name })));
    w.banner.set_revealed(!owned);
    w.switch.set_sensitive(owned);
    w.nets.group.set_sensitive(owned);
    w.cur.set_sensitive(owned);
    if st.state == "connected" {
        w.cur.set_title(&esc(st.ssid.as_deref().unwrap_or("")));
        let band = if st.freq.unwrap_or(0.0) > 4900.0 { "5 GHz" } else { "2.4 GHz" };
        w.cur.set_subtitle(&format!("{} · {} dBm · {band}", st.ip.as_deref().unwrap_or(""), st.signal.unwrap_or(0)));
        w.disc.set_visible(true);
    } else {
        w.cur.set_title("Not connected");
        w.cur.set_subtitle(&st.state);
        w.disc.set_visible(false);
    }
    w.saved.clear();
    for ssid in &st.saved {
        let r = row(ssid, if st.ssid.as_deref() == Some(ssid) { "connected" } else { "" }, Some("network-wireless-symbolic"), false);
        let (ww, ssid) = (w.clone(), ssid.clone());
        r.add_suffix(&button("Forget", None, with(s, move |s| {
            let ww = ww.clone();
            let me = Rc::downgrade(s);
            s.ask_t(Op::WifiForget { ssid: ssid.clone() }, move |st: WifiStatus| {
                if let Some(s) = me.upgrade() {
                    wifi_show(&s, &ww, &st);
                }
            });
        })));
        w.saved.add(&r);
    }
}

fn wifi_scan(s: &S, w: &Rc<Wifi>) {
    w.nets.group.set_description(Some("Scanning…"));
    let (w, me) = (w.clone(), Rc::downgrade(s));
    s.broker.get(Op::WifiScan, move |r: Result<Vec<WifiNetwork>, String>| {
        let Some(s) = me.upgrade() else { return };
        w.nets.group.set_description(None);
        let nets = match r {
            Ok(n) => n,
            Err(e) => return s.toast(&e),
        };
        w.nets.clear();
        for n in nets {
            let bars = match n.signal {
                76.. => "excellent",
                51..=75 => "good",
                26..=50 => "ok",
                _ => "weak",
            };
            let mut sub = format!("{} · {} · {bars}", n.band, if n.security.is_empty() { "open".into() } else { format!("🔒 {}", n.security) });
            if n.active {
                sub = format!("connected · {sub}");
            }
            let r = row(&n.ssid, &sub, Some(&format!("network-wireless-signal-{bars}-symbolic")), !n.active);
            if !n.active {
                let ww = w.clone();
                let join = with(&s, move |s| wifi_join(s, &ww, &n));
                r.connect_activated(move |_| join());
            }
            w.nets.add(&r);
        }
    });
}

fn wifi_join(s: &S, w: &Rc<Wifi>, n: &WifiNetwork) {
    if n.saved {
        return wifi_connect(s, w, &n.ssid, "");
    }
    // never join a new network without the user confirming it (open networks included)
    let body = if n.security.is_empty() { "Open network: traffic is not encrypted. Connect?".to_string() } else { format!("Password ({})", n.security) };
    let dlg = adw::AlertDialog::new(Some(&esc(&n.ssid)), Some(&esc(&body)));
    let pw = adw::PasswordEntryRow::builder().title("Password").build();
    if !n.security.is_empty() {
        let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).build();
        lb.append(&pw);
        dlg.set_extra_child(Some(&lb));
    }
    dlg.add_response("cancel", "Cancel");
    dlg.add_response("connect", "Connect");
    dlg.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
    dlg.set_default_response(Some("connect"));
    let (ww, ssid, p, d) = (w.clone(), n.ssid.clone(), pw.clone(), dlg.clone());
    let go = with(s, move |s| wifi_connect(s, &ww, &ssid, &p.text()));
    let go = Rc::new(go);
    let g2 = go.clone();
    pw.connect_entry_activated(move |_| {
        d.close();
        g2();
    });
    dlg.connect_response(None, move |_, resp| {
        if resp == "connect" {
            go();
        }
    });
    dlg.present(Some(s.window()));
    if !n.security.is_empty() {
        glib::idle_add_local_once(move || {
            pw.grab_focus();
        });
    }
}

fn wifi_connect(s: &S, w: &Rc<Wifi>, ssid: &str, password: &str) {
    s.toast(&format!("Connecting to {ssid}…"));
    let (w, me, name) = (w.clone(), Rc::downgrade(s), ssid.to_string());
    s.broker.get(Op::WifiConnect { ssid: ssid.into(), password: password.into() }, move |r: Result<WifiStatus, String>| {
        let Some(s) = me.upgrade() else { return };
        match r {
            Ok(st) => {
                s.toast(&format!("Connected to {name}"));
                wifi_show(&s, &w, &st);
                wifi_scan(&s, &w);
            }
            Err(e) => s.toast(&format!("{name}: {e}")),
        }
    });
}

pub fn wifi_page(s: &S) -> adw::NavigationPage {
    let banner = adw::Banner::builder().button_label("Take back").build();
    let g_main = group("", "");
    let switch = adw::SwitchRow::builder().title("Wi-Fi").build();
    g_main.add(&switch);
    let cur = row("Not connected", "", Some("network-wireless-symbolic"), false);
    let disc = gtk::Button::builder().label("Disconnect").valign(gtk::Align::Center).build();
    cur.add_suffix(&disc);
    g_main.add(&cur);
    let w = Rc::new(Wifi { banner, switch, cur, disc, nets: Rows::new(group("Networks", "")), saved: Rows::new(group("Saved networks", "")), updating: Cell::new(false) });
    let scan = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Scan").build();
    let p = page("Wi-Fi", &[&g_main, &w.nets.group, &w.saved.group], Some(scan.upcast_ref()));
    p.toolbar.add_top_bar(&w.banner);
    let ww = w.clone();
    let f = with(s, move |s| wifi_scan(s, &ww));
    scan.connect_clicked(move |_| f());
    let ww = w.clone();
    let f = with(s, move |s| {
        let ww2 = ww.clone();
        let me = Rc::downgrade(s);
        s.ask(Op::HandoverTake { items: vec!["internet".into()] }, move |_| {
            if let Some(s) = me.upgrade() {
                s.toast("Wi-Fi is back with mothershipOS");
                wifi_reload(&s, &ww2);
            }
        });
    });
    w.banner.connect_button_clicked(move |_| f());
    let ww = w.clone();
    let f = with(s, move |s| {
        if ww.updating.get() {
            return;
        }
        let ww2 = ww.clone();
        let me = Rc::downgrade(s);
        s.ask_t(Op::WifiRadio { on: ww.switch.is_active() }, move |st: WifiStatus| {
            if let Some(s) = me.upgrade() {
                wifi_show(&s, &ww2, &st);
            }
        });
    });
    w.switch.connect_active_notify(move |_| f());
    let ww = w.clone();
    let f = with(s, move |s| {
        let ww2 = ww.clone();
        let me = Rc::downgrade(s);
        s.ask_t(Op::WifiDisconnect, move |st: WifiStatus| {
            if let Some(s) = me.upgrade() {
                wifi_show(&s, &ww2, &st);
            }
        });
    });
    w.disc.connect_clicked(move |_| f());
    wifi_reload(s, &w);
    wifi_scan(s, &w);
    let ww = w.clone();
    s.live(&p.page, 4, with(s, move |s| wifi_reload(s, &ww)));
    p.page
}

// ------------------------------------------------------------------ SSH
struct Ssh {
    on: adw::SwitchRow,
    listen: adw::ComboRow,
    pw: adw::SwitchRow,
    root: adw::SwitchRow,
    addr: Rows,
    keys: adw::ActionRow,
    host: Rows,
    updating: Cell<bool>,
}

fn ssh_show(x: &Ssh, st: &SshStatus) {
    x.updating.set(true);
    x.on.set_active(st.conf.enabled);
    x.listen.set_selected(if st.conf.listen == "usb" { 0 } else { 1 });
    x.pw.set_active(st.conf.password_auth);
    x.root.set_active(st.conf.root_login);
    x.updating.set(false);
    for w in [x.listen.upcast_ref::<gtk::Widget>(), x.pw.upcast_ref(), x.root.upcast_ref()] {
        w.set_sensitive(st.conf.enabled);
    }
    x.addr.clear();
    if st.addresses.is_empty() {
        x.addr.add(&row("server is off", "", Some("network-server-symbolic"), false));
    }
    for a in &st.addresses {
        let (ip, rest) = a.split_once(' ').unwrap_or((a, ""));
        let r = row(&format!("ssh mship@{ip}"), rest, Some("network-server-symbolic"), false);
        r.set_subtitle_selectable(true);
        x.addr.add(&r);
    }
    let keys = st.keys.get("mship").cloned().unwrap_or_default();
    x.keys.set_subtitle(&esc(&if keys.is_empty() { "none".into() } else { keys.join(", ") }));
    x.host.clear();
    for f in &st.hostkeys {
        let (t, fp) = f.split_once(' ').unwrap_or((f, ""));
        let r = row(t, fp, None, false);
        r.set_subtitle_selectable(true);
        x.host.add(&r);
    }
}

pub fn ssh_page(s: &S) -> adw::NavigationPage {
    let g = group("Server", "");
    let x = Rc::new(Ssh {
        on: adw::SwitchRow::builder().title("SSH server").subtitle("OpenSSH, port 22").build(),
        listen: adw::ComboRow::builder().title("Reachable from").model(&strings(&["USB only", "USB and Wi-Fi"])).build(),
        pw: adw::SwitchRow::builder().title("Password login").subtitle("off = public keys only").build(),
        root: adw::SwitchRow::builder().title("Root login").subtitle("root can log in with a key (never a password)").build(),
        addr: Rows::new(group("Connect with", "")),
        keys: row("Authorized keys", "", None, false),
        host: Rows::new(group("Host key fingerprints", "")),
        updating: Cell::new(true),
    });
    g.add(&x.on);
    g.add(&x.listen);
    g.add(&x.pw);
    g.add(&x.root);
    let set = {
        let x = x.clone();
        move |s: &S, key: &'static str, value: String| {
            if x.updating.get() {
                return;
            }
            let (xx, me) = (x.clone(), Rc::downgrade(s));
            s.ask_t(Op::SshSet { key: key.into(), value }, move |st: SshStatus| {
                if me.upgrade().is_some() {
                    ssh_show(&xx, &st);
                }
            });
        }
    };
    let set = Rc::new(set);
    let onoff = |b: bool| if b { "on".to_string() } else { "off".to_string() };
    for (w, key) in [(&x.on, "enabled"), (&x.pw, "password-auth"), (&x.root, "root-login")] {
        let set = set.clone();
        let me = Rc::downgrade(s);
        w.connect_active_notify(move |w| {
            if let Some(s) = me.upgrade() {
                set(&s, key, onoff(w.is_active()));
            }
        });
    }
    let (set2, me) = (set.clone(), Rc::downgrade(s));
    x.listen.connect_selected_notify(move |w| {
        if let Some(s) = me.upgrade() {
            set2(&s, "listen", if w.selected() == 0 { "usb".into() } else { "all".into() });
        }
    });
    let g_users = group("User mship", "");
    let pw = adw::PasswordEntryRow::builder().title("Set new password").show_apply_button(true).build();
    let me = Rc::downgrade(s);
    pw.connect_apply(move |w| {
        if let Some(s) = me.upgrade() {
            let me2 = Rc::downgrade(&s);
            s.ask(Op::SshPasswd { user: "mship".into(), password: w.text().into() }, move |_| {
                if let Some(s) = me2.upgrade() {
                    s.toast("Password set");
                }
            });
            w.set_text("");
        }
    });
    let key = adw::EntryRow::builder().title("Add public key (ssh-ed25519 AAAA…)").show_apply_button(true).build();
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    key.connect_apply(move |w| {
        if let Some(s) = me.upgrade() {
            let (xx, me2) = (xx.clone(), Rc::downgrade(&s));
            s.ask_t(Op::SshAddKey { user: "mship".into(), key: w.text().into() }, move |st: SshStatus| {
                if let Some(s) = me2.upgrade() {
                    s.toast("Key added");
                    ssh_show(&xx, &st);
                }
            });
            w.set_text("");
        }
    });
    g_users.add(&pw);
    g_users.add(&key);
    g_users.add(&x.keys);
    let p = page("SSH", &[&g, &x.addr.group, &g_users, &x.host.group], None);
    let xx = x.clone();
    s.ask_t(Op::SshStatus, move |st: SshStatus| ssh_show(&xx, &st));
    p.page
}

// ------------------------------------------------------------------ CPU
const CLUSTERS: &[(&str, &str)] = &[("policy0", "Efficiency cores (A55 ×4)"), ("policy4", "Performance cores (A77 ×3)"), ("policy7", "Prime core (A77)")];

fn profile_info(p: &str) -> &'static str {
    match p {
        "cool" => "Coolest and longest battery: lower clocks, ramps up only under heavy load and drops back quickly; slows down further above 70 °C.",
        "balanced" => "Scales with the load, prime core a little below its top clock; slows down above 80 °C.",
        "performance" => "Every core at full clock all the time. Runs hot: only the kernel slows it down, at 90 °C.",
        _ => "Set per cluster below; no temperature limit.",
    }
}

struct Cluster {
    policy: &'static str,
    cur: adw::ActionRow,
    gov: adw::ComboRow,
    max: adw::ComboRow,
    govs: RefCell<Vec<String>>,
    freqs: RefCell<Vec<u64>>,
}

struct Cpu {
    prof: adw::ComboRow,
    info: adw::ActionRow,
    clusters: Vec<Cluster>,
    temps: Rows,
    updating: Cell<bool>,
}

fn cpu_show(x: &Cpu, c: &CpuStatus) {
    x.updating.set(true);
    let prof = c.profile.as_deref().unwrap_or("custom");
    let idx = PROFILES.iter().position(|p| *p == prof);
    x.prof.set_selected(idx.unwrap_or(3) as u32);
    let mut info = profile_info(if idx.is_some() { prof } else { "custom" }).to_string();
    if c.throttled {
        info += &format!(" Throttling now: {:.0} °C.", c.hottest.unwrap_or(0.0));
    }
    x.info.set_title(&esc(&info));
    for p in &c.policies {
        let Some(g) = x.clusters.iter().find(|g| g.policy == p.policy) else { continue };
        g.cur.set_subtitle(&format!("{}  (hardware {} – {})", mhz(p.cur), mhz(p.hw_min), mhz(p.hw_max)));
        if *g.govs.borrow() != p.governors {
            *g.govs.borrow_mut() = p.governors.clone();
            g.gov.set_model(Some(&strings(&p.governors.iter().map(String::as_str).collect::<Vec<_>>())));
        }
        if let Some(i) = p.governors.iter().position(|x| *x == p.governor) {
            g.gov.set_selected(i as u32);
        }
        if *g.freqs.borrow() != p.freqs {
            *g.freqs.borrow_mut() = p.freqs.clone();
            let labels: Vec<String> = p.freqs.iter().map(|f| mhz(*f)).collect();
            g.max.set_model(Some(&strings(&labels.iter().map(String::as_str).collect::<Vec<_>>())));
        }
        if let Some(i) = p.freqs.iter().position(|f| *f == p.max) {
            g.max.set_selected(i as u32);
        }
    }
    x.updating.set(false);
    x.temps.clear();
    let mut zones: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for (k, v) in &c.temps {
        let b = k.split('-').next().unwrap_or(k).to_string();
        let e = zones.entry(b).or_insert(*v);
        *e = e.max(*v);
    }
    let mut keys: Vec<&String> = zones.keys().collect();
    keys.sort_by_key(|k| (!k.starts_with("cpu"), (*k).clone()));
    for k in keys {
        x.temps.add(&row(&k.to_uppercase().replace("CPU", "CPU "), &format!("{:.0} °C", zones[k]), None, false));
    }
}

pub fn cpu_page(s: &S) -> adw::NavigationPage {
    let mut names: Vec<String> = PROFILES.iter().map(|p| profile_name(p)).collect();
    names.push(profile_name("custom"));
    let prof = adw::ComboRow::builder().title("Mode").model(&strings(&names.iter().map(String::as_str).collect::<Vec<_>>())).build();
    let info = row("…", "", None, false);
    info.set_title_lines(0);
    let g = group("Profile", "");
    g.add(&prof);
    g.add(&info);
    let mut groups = vec![g];
    let mut clusters = vec![];
    for (p, name) in CLUSTERS {
        let cg = group(name, "");
        let c = Cluster {
            policy: p,
            cur: row("Current frequency", "…", None, false),
            gov: adw::ComboRow::builder().title("Governor").build(),
            max: adw::ComboRow::builder().title("Maximum frequency").build(),
            govs: RefCell::default(),
            freqs: RefCell::default(),
        };
        cg.add(&c.cur);
        cg.add(&c.gov);
        cg.add(&c.max);
        groups.push(cg);
        clusters.push(c);
    }
    let x = Rc::new(Cpu { prof, info, clusters, temps: Rows::new(group("Temperatures", "")), updating: Cell::new(false) });
    groups.push(x.temps.group.clone());
    let refs: Vec<&adw::PreferencesGroup> = groups.iter().collect();
    let p = page("CPU & performance", &refs, None);
    let xx = x.clone();
    let f = with(s, move |s| {
        let i = xx.prof.selected() as usize;
        if xx.updating.get() || i >= PROFILES.len() {
            return;
        }
        let xx2 = xx.clone();
        s.ask_t(Op::CpuProfile { name: PROFILES[i].into() }, move |c: CpuStatus| cpu_show(&xx2, &c));
    });
    x.prof.connect_selected_notify(move |_| f());
    for i in 0..x.clusters.len() {
        let xx = x.clone();
        let f = Rc::new(with(s, move |s| {
            let c = &xx.clusters[i];
            let (govs, freqs) = (c.govs.borrow().clone(), c.freqs.borrow().clone());
            if xx.updating.get() || govs.is_empty() || freqs.is_empty() {
                return;
            }
            let (Some(gov), Some(max)) = (govs.get(c.gov.selected() as usize), freqs.get(c.max.selected() as usize)) else { return };
            let xx2 = xx.clone();
            s.ask_t(Op::CpuSet { policy: c.policy.into(), governor: gov.clone(), max_khz: *max }, move |st: CpuStatus| cpu_show(&xx2, &st));
        }));
        let f2 = f.clone();
        x.clusters[i].gov.connect_selected_notify(move |_| f());
        x.clusters[i].max.connect_selected_notify(move |_| f2());
    }
    let xx = x.clone();
    s.ask_t(Op::CpuStatus, move |c: CpuStatus| cpu_show(&xx, &c));
    // live detail only while this page is open
    s.broker.subscribe(Topic::Cpu, true);
    let xx = x.clone();
    let l = s.broker.listen(Topic::Cpu, move |v| {
        if let Some(c) = v.and_then(|v| serde_json::from_value::<CpuStatus>(v.clone()).ok()) {
            cpu_show(&xx, &c);
        }
    });
    let me = Rc::downgrade(s);
    s.on_gone(&p.page, move || {
        l.stop();
        if let Some(s) = me.upgrade() {
            s.broker.subscribe(Topic::Cpu, false);
        }
    });
    p.page
}

// ------------------------------------------------------------------ Bluetooth
struct Bt {
    on: adw::SwitchRow,
    vis: adw::SwitchRow,
    addr: adw::ActionRow,
    paired: Rows,
    avail: Rows,
    updating: Cell<bool>,
}

fn bt_op(s: &S, x: &Rc<Bt>, op: Op, done: Option<String>) {
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    s.ask_t(op, move |st: BtStatus| {
        if let Some(s) = me.upgrade() {
            if let Some(m) = &done {
                s.toast(m);
            }
            bt_show(&s, &xx, &st);
        }
    });
}

fn bt_show(s: &S, x: &Rc<Bt>, st: &BtStatus) {
    if !st.present {
        // lent: the controller is the VM's (mship.bt)
        let owner = s.handover().holders.get("bluetooth").cloned().unwrap_or_else(|| "host".into());
        x.addr.set_subtitle(&esc(&if owner != "host" { format!("lent to {}", vm_name(base(&owner))) } else { "no Bluetooth controller".into() }));
        for w in [x.on.upcast_ref::<gtk::Widget>(), x.vis.upcast_ref(), x.paired.group.upcast_ref(), x.avail.group.upcast_ref()] {
            w.set_sensitive(false);
        }
        return;
    }
    x.on.set_sensitive(true);
    let on = st.powered.as_deref() == Some("yes");
    x.updating.set(true);
    x.on.set_active(on);
    x.vis.set_active(st.discoverable.as_deref() == Some("yes"));
    x.updating.set(false);
    for w in [x.vis.upcast_ref::<gtk::Widget>(), x.paired.group.upcast_ref(), x.avail.group.upcast_ref()] {
        w.set_sensitive(on);
    }
    x.addr.set_subtitle(&esc(&format!("{} · {}", st.alias.as_deref().unwrap_or(""), st.address.as_deref().unwrap_or(""))));
    x.paired.clear();
    for d in &st.paired {
        let conn = d.connected.as_deref() == Some("yes");
        let name = d.alias.clone().or(d.name.clone()).unwrap_or_else(|| d.mac.clone());
        let r = row(&name, if conn { "connected" } else { "not connected" }, Some("bluetooth-active-symbolic"), false);
        let mac = d.mac.clone();
        let xx = x.clone();
        if conn {
            r.add_suffix(&button("Disconnect", None, with(s, move |s| bt_op(s, &xx, Op::BtDisconnect { mac: mac.clone() }, None))));
        } else {
            r.add_suffix(&button("Connect", Some("suggested-action"), with(s, move |s| {
                s.toast("Connecting…");
                bt_op(s, &xx, Op::BtConnect { mac: mac.clone() }, None);
            })));
        }
        let (xx, mac) = (x.clone(), d.mac.clone());
        r.add_suffix(&button("Forget", None, with(s, move |s| bt_op(s, &xx, Op::BtRemove { mac: mac.clone() }, None))));
        x.paired.add(&r);
    }
    if st.paired.is_empty() {
        x.paired.add(&row("No paired devices", "pair one from the list below", None, false));
    }
}

fn bt_scan(s: &S, x: &Rc<Bt>) {
    x.avail.group.set_description(Some("Scanning (10 s)…"));
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    s.broker.get(Op::BtScan, move |r: Result<Vec<BtFound>, String>| {
        let Some(s) = me.upgrade() else { return };
        xx.avail.group.set_description(None);
        let found = match r {
            Ok(f) => f,
            Err(e) => return s.toast(&e),
        };
        xx.avail.clear();
        for d in &found {
            let sub = format!("{}{}", d.mac, d.rssi.map_or(String::new(), |r| format!(" · {r} dBm")));
            let r = row(if d.named { &d.name } else { "Unnamed device" }, &sub, Some("bluetooth-active-symbolic"), true);
            let (x2, d2) = (xx.clone(), d.clone());
            let f = with(&s, move |s| bt_pair(s, &x2, &d2));
            r.connect_activated(move |_| f());
            xx.avail.add(&r);
        }
        if found.is_empty() {
            xx.avail.add(&row("Nothing found", "put the other device in pairing mode and scan again", None, false));
        }
    });
}

fn bt_pair(s: &S, x: &Rc<Bt>, d: &BtFound) {
    let name = if d.named { d.name.clone() } else { d.mac.clone() };
    let dlg = adw::AlertDialog::new(Some(&esc(&format!("Pair with {name}?"))), Some("The other device must be in pairing mode."));
    dlg.add_response("cancel", "Cancel");
    dlg.add_response("pair", "Pair");
    dlg.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
    let (xx, mac, me) = (x.clone(), d.mac.clone(), Rc::downgrade(s));
    dlg.connect_response(None, move |_, resp| {
        let Some(s) = me.upgrade() else { return };
        if resp != "pair" {
            return;
        }
        s.toast(&format!("Pairing with {name}…"));
        let (x2, n2, me2) = (xx.clone(), name.clone(), Rc::downgrade(&s));
        s.broker.get(Op::BtPair { mac: mac.clone() }, move |r: Result<BtStatus, String>| {
            let Some(s) = me2.upgrade() else { return };
            match r {
                Ok(st) => {
                    s.toast(&format!("Paired with {n2}"));
                    bt_show(&s, &x2, &st);
                    bt_scan(&s, &x2);
                }
                Err(e) => s.toast(&format!("{n2}: {e}")),
            }
        });
    });
    dlg.present(Some(s.window()));
}

pub fn bt_page(s: &S) -> adw::NavigationPage {
    let x = Rc::new(Bt {
        on: adw::SwitchRow::builder().title("Bluetooth").build(),
        vis: adw::SwitchRow::builder().title("Visible to other devices").subtitle("lets phones and computers find and pair with this device").build(),
        addr: row("This device", "…", Some("bluetooth-active-symbolic"), false),
        paired: Rows::new(group("My devices", "")),
        avail: Rows::new(group("Available devices", "")),
        updating: Cell::new(false),
    });
    let g = group("", "");
    g.add(&x.on);
    g.add(&x.vis);
    g.add(&x.addr);
    let scan = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Scan").build();
    let p = page("Bluetooth", &[&g, &x.paired.group, &x.avail.group], Some(scan.upcast_ref()));
    let xx = x.clone();
    let f = with(s, move |s| bt_scan(s, &xx));
    scan.connect_clicked(move |_| f());
    let xx = x.clone();
    let f = with(s, move |s| {
        if !xx.updating.get() {
            bt_op(s, &xx, Op::BtPower { on: xx.on.is_active() }, None);
        }
    });
    x.on.connect_active_notify(move |_| f());
    let xx = x.clone();
    let f = with(s, move |s| {
        if !xx.updating.get() {
            bt_op(s, &xx, Op::BtDiscoverable { on: xx.vis.is_active() }, None);
        }
    });
    x.vis.connect_active_notify(move |_| f());
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    s.ask_t(Op::BtStatus, move |st: BtStatus| {
        if let Some(s) = me.upgrade() {
            bt_show(&s, &xx, &st);
            if st.powered.as_deref() == Some("yes") {
                bt_scan(&s, &xx);
            }
        }
    });
    let xx = x.clone();
    s.live(&p.page, 4, with(s, move |s| {
        let (x2, me) = (xx.clone(), Rc::downgrade(s));
        s.broker.get(Op::BtStatus, move |r: Result<BtStatus, String>| {
            if let (Ok(st), Some(s)) = (r, me.upgrade()) {
                bt_show(&s, &x2, &st);
            }
        });
    }));
    p.page
}

// ------------------------------------------------------------------ battery
const LIMITS: &[(u32, &str)] = &[(100, "Off (charge to 100 %)"), (90, "90 %"), (85, "85 %"), (80, "80 % (best for battery life)"), (70, "70 %"), (60, "60 %")];

struct Battery {
    level: gtk::LevelBar,
    lvrow: adw::ActionRow,
    state: adw::ActionRow,
    power: adw::ActionRow,
    temp: adw::ActionRow,
    chg: adw::ActionRow,
    rate: adw::ActionRow,
    limit: adw::ComboRow,
    on: adw::SwitchRow,
    info: adw::ActionRow,
    updating: Cell<bool>,
}

fn battery_show(x: &Battery, b: &BatteryStatus) {
    if !b.present {
        x.state.set_subtitle("battery driver not loaded");
        return;
    }
    x.updating.set(true);
    let cap = b.capacity.unwrap_or(0);
    x.level.set_value(cap as f64);
    x.lvrow.set_subtitle(&format!("{cap} %"));
    let status = b.status.clone().unwrap_or_default();
    let mut st = match status.as_str() {
        "Charging" => "Charging",
        "Discharging" => "On battery",
        "Full" => "Full",
        "Not charging" => "Plugged in, not charging",
        s => s,
    }
    .to_string();
    if status == "Charging" && b.current < 0.0 {
        st = "Plugged in, but using more than the charger supplies".into();
    }
    x.state.set_subtitle(&format!("{st} · health {}", b.health.clone().unwrap_or_default().to_lowercase()));
    x.power.set_subtitle(&format!("{:+.0} mA · {:.3} V · {:+.2} W", b.current, b.voltage, b.power));
    x.temp.set_subtitle(&format!("{:.1} °C", b.temp));
    match b.charger.as_ref().filter(|c| c.online) {
        Some(ch) => {
            let kind = match ch.kind.as_deref() {
                Some("SDP") => "USB port (PC)",
                Some("CDP") => "USB charging port",
                Some("DCP") => "wall charger",
                Some("C") => "USB-C charger",
                Some(k) => k,
                None => "unknown",
            };
            x.chg.set_subtitle(&format!("{kind} · {:.2} V · limit {} mA", ch.voltage, ch.input_limit_ma));
            x.rate.set_subtitle(&if ch.charge_current_ma != 0 {
                format!("{} mA allowed · {}", ch.charge_current_ma, ch.charge_type.clone().unwrap_or_default().to_lowercase())
            } else {
                "paused (limit, temperature or switched off)".into()
            });
        }
        None => {
            x.chg.set_subtitle("not connected");
            x.rate.set_subtitle("—");
        }
    }
    x.limit.set_selected(LIMITS.iter().position(|l| l.0 == b.conf.end_threshold).unwrap_or(0) as u32);
    x.on.set_active(b.conf.charging);
    x.updating.set(false);
    x.info.set_subtitle(&format!("Li-ion, {} mAh design, max 4.45 V", b.full_design_mah));
}

pub fn battery_page(s: &S) -> adw::NavigationPage {
    let level = gtk::LevelBar::builder().min_value(0.0).max_value(100.0).hexpand(true).valign(gtk::Align::Center).build();
    level.set_size_request(-1, 14);
    let lvrow = row("Charge", "…", Some("battery-good-symbolic"), false);
    lvrow.add_suffix(&level);
    let x = Rc::new(Battery {
        level,
        lvrow,
        state: row("Status", "…", None, false),
        power: row("Power", "…", None, false),
        temp: row("Temperature", "…", None, false),
        chg: row("Input", "…", Some("ac-adapter-symbolic"), false),
        rate: row("Charge current", "…", None, false),
        limit: adw::ComboRow::builder().title("Charge limit").subtitle("stop charging at this level").model(&strings(&LIMITS.iter().map(|l| l.1).collect::<Vec<_>>())).build(),
        on: adw::SwitchRow::builder().title("Charging").subtitle("off = run from the charger without charging the battery").build(),
        info: row("Cell", "…", None, false),
        updating: Cell::new(false),
    });
    let g = group("", "");
    for w in [&x.lvrow, &x.state, &x.power, &x.temp] {
        g.add(w);
    }
    let gc = group("Charger", "");
    gc.add(&x.chg);
    gc.add(&x.rate);
    let gs = group("Charging", "The charge current follows the battery temperature (stock Motorola zones); charging stops below 0 °C and above 60 °C.");
    gs.add(&x.limit);
    gs.add(&x.on);
    let gi = group("Battery", "");
    gi.add(&x.info);
    let set = |x: &Rc<Battery>, s: &S, key: &str, value: String| {
        if x.updating.get() {
            return;
        }
        let xx = x.clone();
        s.ask_t(Op::BatterySet { key: key.into(), value }, move |b: BatteryStatus| battery_show(&xx, &b));
    };
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    x.limit.connect_selected_notify(move |w| {
        if let Some(s) = me.upgrade() {
            set(&xx, &s, "end_threshold", LIMITS[w.selected() as usize].0.to_string());
        }
    });
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    x.on.connect_active_notify(move |w| {
        if let Some(s) = me.upgrade() {
            set(&xx, &s, "charging", if w.is_active() { "on".into() } else { "off".into() });
        }
    });
    let p = page("Battery", &[&g, &gc, &gs, &gi], None);
    let xx = x.clone();
    s.ask_t(Op::BatteryStatus, move |b: BatteryStatus| battery_show(&xx, &b));
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    s.follow(&p.page, &[Topic::Host], move || {
        if let Some(st) = me.upgrade().and_then(|s| s.broker.state(Topic::Host)).and_then(|v| serde_json::from_value::<HostStatus>(v).ok()) {
            battery_show(&xx, &st.battery);
        }
    });
    p.page
}

// ------------------------------------------------------------------ region & time
struct Region {
    country: adw::ActionRow,
    regdom: adw::ActionRow,
    zone: adw::ActionRow,
    h24: adw::SwitchRow,
    now: adw::ActionRow,
    countries: RefCell<Vec<Country>>,
    cur: RefCell<Option<RegionStatus>>,
    updating: Cell<bool>,
}

fn region_show(s: &S, x: &Region, r: RegionStatus) {
    s.clock24.set(r.conf.clock24);
    x.updating.set(true);
    let name = x.countries.borrow().iter().find(|c| c.code == r.conf.country).map(|c| c.name.clone()).unwrap_or(r.conf.country.clone());
    x.country.set_subtitle(&esc(&name));
    x.zone.set_subtitle(&r.timezone.replace('_', " "));
    x.h24.set_active(r.conf.clock24);
    x.updating.set(false);
    x.regdom.set_subtitle(r.wifi_regdom.as_deref().unwrap_or("unknown"));
    x.now.set_subtitle(&r.time);
    *x.cur.borrow_mut() = Some(r);
}

fn region_op(s: &S, x: &Rc<Region>, op: Op) {
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    s.ask_t(op, move |r: RegionStatus| {
        if let Some(s) = me.upgrade() {
            region_show(&s, &xx, r);
        }
    });
}

pub fn region_page(s: &S) -> adw::NavigationPage {
    let x = Rc::new(Region {
        country: row("Country or region", "…", Some("mark-location-symbolic"), true),
        regdom: row("Wi-Fi regulatory domain", "…", Some("network-wireless-symbolic"), false),
        zone: row("Time zone", "…", Some("preferences-system-time-symbolic"), true),
        h24: adw::SwitchRow::builder().title("24-hour clock").build(),
        now: row("Current time", "…", Some("preferences-system-time-symbolic"), false),
        countries: RefCell::default(),
        cur: RefCell::default(),
        updating: Cell::new(true),
    });
    let g = group("Country", "Sets the Wi-Fi regulatory domain: allowed channels and transmit power.");
    g.add(&x.country);
    g.add(&x.regdom);
    let gt = group("Time", "");
    gt.add(&x.zone);
    gt.add(&x.h24);
    gt.add(&x.now);
    let xx = x.clone();
    let f = with(s, move |s| {
        let items = xx.countries.borrow().iter().map(|c| (if c.code == "00" { c.name.clone() } else { format!("{} ({})", c.name, c.code) }, c.code.clone())).collect();
        let cur = xx.cur.borrow().as_ref().map(|r| r.conf.country.clone()).unwrap_or_default();
        let x2 = xx.clone();
        pick(s, "Country or region", items, &cur, move |s, code| {
            let Some(c) = x2.countries.borrow().iter().find(|c| c.code == code).cloned() else { return };
            let (x3, me) = (x2.clone(), Rc::downgrade(s));
            s.ask_t(Op::RegionCountry { code: c.code.clone() }, move |r: RegionStatus| {
                let Some(s) = me.upgrade() else { return };
                // a new country: its first time zone unless the current one already belongs to it
                if let Some(z) = c.zones.first().filter(|_| !c.zones.contains(&r.timezone)) {
                    region_op(&s, &x3, Op::RegionTimezone { tz: z.clone() });
                } else {
                    region_show(&s, &x3, r);
                }
                s.toast(&format!("Region: {}", c.name));
            });
        });
    });
    x.country.connect_activated(move |_| f());
    let xx = x.clone();
    let f = with(s, move |s| {
        let countries = xx.countries.borrow();
        let cur = xx.cur.borrow().clone();
        let country = cur.as_ref().map(|r| r.conf.country.clone()).unwrap_or_default();
        let mut zones: Vec<String> = countries.iter().find(|c| c.code == country).map(|c| c.zones.clone()).unwrap_or_default();
        let mut all: Vec<String> = countries.iter().flat_map(|c| c.zones.clone()).collect();
        all.sort();
        all.dedup();
        zones.push("Etc/UTC".into());
        zones.extend(all);
        let items = zones.into_iter().map(|z| (z.replace('_', " "), z)).collect();
        let x2 = xx.clone();
        pick(s, "Time zone", items, cur.as_ref().map_or("", |r| r.timezone.as_str()), move |s, tz| region_op(s, &x2, Op::RegionTimezone { tz }));
    });
    x.zone.connect_activated(move |_| f());
    let xx = x.clone();
    let f = with(s, move |s| {
        if !xx.updating.get() {
            region_op(s, &xx, Op::RegionClock24 { on: xx.h24.is_active() });
        }
    });
    x.h24.connect_active_notify(move |_| f());
    let p = page("Region & time", &[&g, &gt], None);
    let (xx, me) = (x.clone(), Rc::downgrade(s));
    s.ask_t(Op::RegionCountries, move |cs: Vec<Country>| {
        let mut all = vec![Country { code: "00".into(), name: "World (most restrictive)".into(), zones: vec![] }];
        all.extend(cs);
        *xx.countries.borrow_mut() = all;
        if let Some(s) = me.upgrade() {
            region_op(&s, &xx, Op::RegionStatus);
        }
    });
    let xx = x.clone();
    s.live(&p.page, 4, with(s, move |s| {
        let (x2, me) = (xx.clone(), Rc::downgrade(s));
        s.broker.get(Op::RegionStatus, move |r: Result<RegionStatus, String>| {
            if let (Ok(r), Some(s)) = (r, me.upgrade()) {
                region_show(&s, &x2, r);
            }
        });
    }));
    p.page
}

// ------------------------------------------------------------------ display
const TIMEOUTS: &[(u64, &str)] = &[(15, "15 seconds"), (30, "30 seconds"), (60, "1 minute"), (120, "2 minutes"), (300, "5 minutes"), (0, "Never")];
const SCALES: &[&str] = &["1.5", "2", "2.5", "3"];

struct Display {
    to: adw::ComboRow,
    sleep: adw::SwitchRow,
    usb: adw::SwitchRow,
    st: adw::ActionRow,
    updating: Cell<bool>,
}

fn display_show(x: &Display, c: &PowerStatus) {
    x.updating.set(true);
    x.to.set_selected(TIMEOUTS.iter().position(|t| t.0 == c.conf.screen_timeout).unwrap_or(2) as u32);
    x.sleep.set_active(c.conf.sleep);
    x.usb.set_active(c.conf.stay_awake_usb);
    x.usb.set_sensitive(c.conf.sleep);
    x.st.set_subtitle(&esc(&format!("{} suspends this boot · {}", c.suspends, if c.sleepd.is_empty() { "idle" } else { &c.sleepd })));
    x.updating.set(false);
}

pub fn display_page(s: &S) -> adw::NavigationPage {
    let g = group("Panel", "");
    let br = row("Brightness", "", None, false);
    let sc = gtk::Scale::with_range(gtk::Orientation::Horizontal, 1.0, 100.0, 1.0);
    sc.set_hexpand(true);
    sc.set_size_request(260, -1);
    sc.set_value(s.status.borrow().as_ref().map_or(50, |st| st.brightness) as f64);
    // at most every 120 ms while dragging
    let pending = Rc::new(Cell::new(false));
    let (b2, me) = (br.clone(), Rc::downgrade(s));
    sc.connect_value_changed(move |sc| {
        b2.set_subtitle(&format!("{} %", sc.value() as u32));
        if pending.replace(true) {
            return;
        }
        let (sc, p, me) = (sc.clone(), pending.clone(), me.clone());
        glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
            p.set(false);
            if let Some(s) = me.upgrade() {
                s.ask(Op::BrightnessSet { percent: sc.value() as u32 }, |_| {});
            }
        });
    });
    br.add_suffix(&sc);
    g.add(&br);
    let scale = adw::ComboRow::builder().title("Interface scale").subtitle("mothershipOS (VMs always run 1:1)").model(&strings(&["1.5×", "2× (default)", "2.5×", "3×"])).build();
    scale.set_selected(1);
    let me = Rc::downgrade(s);
    scale.connect_selected_notify(move |w| {
        let r = mship_core::sway::Sway::session().and_then(|mut sw| sw.command(&format!("output DSI-1 scale {}", SCALES[w.selected() as usize])));
        if let (Err(e), Some(s)) = (r, me.upgrade()) {
            s.toast(&e.to_string());
        }
    });
    g.add(&scale);

    let gp = group("Screen & sleep", "The power key turns the screen off and on.");
    let x = Rc::new(Display {
        to: adw::ComboRow::builder().title("Turn screen off after").model(&strings(&TIMEOUTS.iter().map(|t| t.1).collect::<Vec<_>>())).build(),
        sleep: adw::SwitchRow::builder().title("Sleep when the screen is off").subtitle("when every VM agrees (Power votes)").build(),
        usb: adw::SwitchRow::builder().title("Stay awake while connected to a PC").subtitle("keeps the USB dev link (SSH) alive").build(),
        st: row("Sleep status", "…", Some("weather-clear-night-symbolic"), false),
        updating: Cell::new(true),
    });
    gp.add(&x.to);
    gp.add(&x.sleep);
    gp.add(&x.usb);
    gp.add(&x.st);
    let setp = Rc::new(move |x: &Rc<Display>, s: &S, key: &str, value: String| {
        if x.updating.get() {
            return;
        }
        let xx = x.clone();
        s.ask_t(Op::PowerSet { key: key.into(), value }, move |c: PowerStatus| display_show(&xx, &c));
    });
    let (xx, me, f) = (x.clone(), Rc::downgrade(s), setp.clone());
    x.to.connect_selected_notify(move |w| {
        if let Some(s) = me.upgrade() {
            f(&xx, &s, "screen_timeout", TIMEOUTS[w.selected() as usize].0.to_string());
        }
    });
    let (xx, me, f) = (x.clone(), Rc::downgrade(s), setp.clone());
    x.sleep.connect_active_notify(move |w| {
        if let Some(s) = me.upgrade() {
            f(&xx, &s, "sleep", if w.is_active() { "on".into() } else { "off".into() });
        }
    });
    let (xx, me, f) = (x.clone(), Rc::downgrade(s), setp.clone());
    x.usb.connect_active_notify(move |w| {
        if let Some(s) = me.upgrade() {
            f(&xx, &s, "stay_awake_usb", if w.is_active() { "on".into() } else { "off".into() });
        }
    });
    let off = adw::ButtonRow::builder().title("Turn screen off now").build();
    let f = with(s, |s| s.ask(Op::ScreenSet { on: false }, |_| {}));
    off.connect_activated(move |_| f());
    gp.add(&off);
    let p = page("Display", &[&g, &gp], None);
    let xx = x.clone();
    s.ask_t(Op::PowerStatus, move |c: PowerStatus| display_show(&xx, &c));
    let xx = x.clone();
    s.live(&p.page, 4, with(s, move |s| {
        let x2 = xx.clone();
        s.broker.get(Op::PowerStatus, move |r: Result<PowerStatus, String>| {
            if let Ok(c) = r {
                display_show(&x2, &c);
            }
        });
    }));
    p.page
}
