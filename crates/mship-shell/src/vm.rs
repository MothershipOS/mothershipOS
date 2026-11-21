//! A VM's page, everything about one VM: start / show / sleep / stop, its hardware ("Use as the phone" and the
//! devices by category), its live CPU / memory / GPU use, its memory limit, its update. Also the hardware overview
//! the Phone page shows.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use mship_core::api::{GroupInfo, Op, PowerState, StoreStatus, Topic, VoteState};
use mship_core::handle::Lend;
use mship_core::status::Resources;
use serde_json::Value;

use crate::system::meminfo;
use crate::ui::{Rows, button, esc, group, page, row};
use crate::{S, Shell, VMS, vm_conf, vm_name};

/// The assignable groups by category, in display order (a group the registry adds later lands in "Other").
const CATEGORIES: &[(&str, &str, &[&str])] = &[
    ("Screen & buttons", "video-display-symbolic", &["display", "buttons"]),
    ("Sound", "audio-speakers-symbolic", &["speaker", "earpiece", "headset", "microphone"]),
    ("Network", "network-wireless-symbolic", &["wifi-2g", "wifi-5g", "cellular", "bluetooth", "internet", "usb"]),
    ("Camera & location", "camera-photo-symbolic", &["cameras", "location"]),
    ("External display", "video-joined-displays-symbolic", &["display-ext"]),
];

/// "Use as the phone" gives a VM every group but these (the USB-C display and the shared internet stay as they
/// are: a second VM can use them)
const NOT_PHONE: &[&str] = &["display-ext", "internet"];

fn owner<'a>(assoc: &'a std::collections::BTreeMap<String, String>, g: &str) -> &'a str {
    assoc.get(g).map(String::as_str).unwrap_or("host")
}

fn owner_name(o: &str) -> String {
    if o == "host" { "mothershipOS".into() } else { vm_name(o) }
}

/// The groups of a category (the "Other" category: every group no category names).
fn in_category<'a>(groups: &'a [GroupInfo], names: Option<&[&str]>) -> Vec<&'a GroupInfo> {
    match names {
        Some(n) => n.iter().filter_map(|x| groups.iter().find(|g| g.name == *x)).collect(),
        None => groups.iter().filter(|g| !CATEGORIES.iter().any(|(_, _, n)| n.contains(&g.name.as_str()))).collect(),
    }
}

fn assign(s: &S, items: Vec<String>, to: &str) {
    for item in items {
        s.ask(Op::HandoverAssign { item, owner: to.to_string() }, |_| {});
    }
}

/// Phone page: who owns what (read only; each VM's row opens its page).
pub fn overview(s: &S, rows: &Rows) {
    rows.clear();
    let h = s.handover();
    let list = s.handles();
    let mut vms: Vec<_> = s.vm_list().into_iter().filter(|v| v.manifest).collect();
    vms.sort_by_key(|v| v.vm.clone());
    let mut owners: Vec<(String, String)> = vms.iter().map(|v| (v.vm.clone(), v.name.clone())).collect();
    owners.push(("host".into(), "mothershipOS".into()));
    for (o, name) in owners {
        let has: Vec<&str> = list.groups.iter().filter(|g| owner(&h.assoc, &g.name) == o).map(|g| g.label.as_str()).collect();
        if o == "host" && has.is_empty() {
            continue;
        }
        let r = row(&name, &if has.is_empty() { "nothing assigned".into() } else { has.join(", ") }, None, o != "host");
        if o != "host" {
            let (me, d) = (Rc::downgrade(s), o.clone());
            r.connect_activated(move |_| {
                if let Some(s) = me.upgrade() {
                    s.push(&vm_page(&s, &d));
                }
            });
        }
        rows.add(&r);
    }
    let shared: Vec<&str> = list.handles.iter().map(|i| &i.handle).filter(|h| h.lend == Lend::EveryVm).map(|h| h.label.as_str()).collect();
    rows.group.set_description(Some(&esc(&if shared.is_empty() { String::new() } else { format!("Every running VM shares: {}.", shared.join(", ")) })));
}

/// The hardware rows of a VM's page, rebuilt on every change (the open categories stay open).
fn hardware(s: &S, d: &str, rows: &Rows, open: &Rc<RefCell<HashSet<String>>>) {
    rows.clear();
    let h = s.handover();
    let list = s.handles();
    let groups = &list.groups;
    let path = format!("{VMS}/{d}");
    let phone: Vec<&GroupInfo> = groups.iter().filter(|g| g.present && !NOT_PHONE.contains(&g.name.as_str())).collect();
    let is_phone = !phone.is_empty() && phone.iter().all(|g| owner(&h.assoc, &g.name) == d);

    let sw = adw::SwitchRow::builder().title("Use as the phone").subtitle(esc("screen, buttons, sound, cameras, location, cellular, Wi-Fi, Bluetooth, USB")).active(is_phone).build();
    sw.add_prefix(&gtk::Image::from_icon_name("phone-symbolic"));
    let (me, dd, items) = (Rc::downgrade(s), d.to_string(), phone.iter().map(|g| g.name.clone()).collect::<Vec<_>>());
    sw.connect_active_notify(move |w| {
        let Some(s) = me.upgrade() else { return };
        if w.is_active() {
            assign(&s, items.clone(), &dd);
            s.toast(&format!("{} is the phone", vm_name(&dd)));
        } else {
            let h = s.handover();
            let mine: Vec<String> = s.handles().groups.iter().filter(|g| owner(&h.assoc, &g.name) == dd).map(|g| g.name.clone()).collect();
            assign(&s, mine, "host");
        }
    });
    rows.add(&sw);

    let cats: Vec<(&str, &str, Option<&[&str]>)> =
        CATEGORIES.iter().map(|(t, i, n)| (*t, *i, Some(*n))).chain(std::iter::once(("Other", "drive-multidisk-symbolic", None))).collect();
    for (title, icon, names) in cats {
        let gs = in_category(groups, names);
        if gs.is_empty() {
            continue;
        }
        let owners: HashSet<&str> = gs.iter().filter(|g| g.present).map(|g| owner(&h.assoc, &g.name)).collect();
        let sum = match owners.len() {
            0 => "not present".to_string(),
            1 => {
                let o = *owners.iter().next().unwrap();
                if o == d { "this VM".into() } else { owner_name(o) }
            }
            _ => "mixed".into(),
        };
        let ex = adw::ExpanderRow::builder().title(esc(title)).subtitle(esc(&sum)).expanded(open.borrow().contains(title)).build();
        ex.add_prefix(&gtk::Image::from_icon_name(icon));
        let (open2, t) = (open.clone(), title.to_string());
        ex.connect_expanded_notify(move |e| {
            if e.is_expanded() {
                open2.borrow_mut().insert(t.clone());
            } else {
                open2.borrow_mut().remove(&t);
            }
        });
        for g in gs {
            let o = owner(&h.assoc, &g.name);
            let in_use = h.holders.get(&g.name) == Some(&path);
            let sub = if !g.present {
                "not present".to_string()
            } else if o == d {
                format!("this VM{}", if in_use { " · in use" } else { "" })
            } else {
                format!("{} has it", owner_name(o))
            };
            let r = adw::SwitchRow::builder().title(esc(&g.label)).subtitle(esc(&sub)).active(o == d).sensitive(g.present).build();
            if !g.icon.is_empty() {
                r.add_prefix(&gtk::Image::from_icon_name(&g.icon));
            }
            let (me, dd, item) = (Rc::downgrade(s), d.to_string(), g.name.clone());
            r.connect_active_notify(move |w| {
                if let Some(s) = me.upgrade() {
                    assign(&s, vec![item.clone()], if w.is_active() { &dd } else { "host" });
                }
            });
            ex.add_row(&r);
        }
        rows.add(&ex);
    }
    let shared: Vec<&str> = list.handles.iter().map(|i| &i.handle).filter(|h| h.lend == Lend::EveryVm).map(|h| h.label.as_str()).collect();
    rows.group.set_description(Some(&esc(&format!(
        "A VM gets its devices while it runs, the screen while it is shown; holding the assist key for 3 s brings the screen back.{}",
        if shared.is_empty() { String::new() } else { format!(" Every VM shares: {}.", shared.join(", ")) }
    ))));
}

/// A VM's RAM limit (MB): vm.json resources.memory_mb, None = the whole pool
fn vm_mem(d: &str) -> Option<u64> {
    vm_conf(d)["resources"]["memory_mb"].as_u64()
}

/// resources.memory_mb into the VM's vm.json (None removes it), keeping everything else (atomic replace)
fn vm_set_mem(d: &str, mb: Option<u64>) -> std::io::Result<()> {
    let path = format!("{VMS}/{d}/vm.json");
    let mut conf = vm_conf(d);
    if !conf.is_object() {
        conf = Value::Object(Default::default());
    }
    if !conf["resources"].is_object() {
        conf["resources"] = Value::Object(Default::default());
    }
    let r = conf["resources"].as_object_mut().unwrap();
    match mb {
        Some(mb) => r.insert("memory_mb".into(), mb.into()),
        None => r.remove("memory_mb"),
    };
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&conf).unwrap_or_default() + "\n")?;
    std::fs::rename(tmp, path)
}

/// The memory limit row of a VM's page.
fn memory_row(s: &S, d: &str) -> adw::ExpanderRow {
    let total = meminfo().get("MemTotal").copied().unwrap_or(0) / 1024;
    let pool = mship_core::pool::vm_size_mb(total, None);
    let limit = vm_mem(d);
    let ex = adw::ExpanderRow::builder().title("Memory limit").show_enable_switch(true).enable_expansion(limit.is_some()).expanded(limit.is_some()).build();
    let running = s.vm_info(d).is_some_and(|v| v.running);
    let sub = move |on: bool| {
        let what = if on { "limited".to_string() } else { format!("none: shares the pool ({:.1} GB)", pool as f64 / 1024.0) };
        format!("{what}{}", if running { " · applies at the next start" } else { "" })
    };
    ex.set_subtitle(&esc(&sub(limit.is_some())));
    let min = mship_core::pool::GUEST_MIN_MB as f64;
    let adj = gtk::Adjustment::new(limit.unwrap_or(pool) as f64, min, pool.max(min as u64) as f64, 256.0, 1024.0, 0.0);
    let sp = adw::SpinRow::new(Some(&adj), 256.0, 0);
    sp.set_title("Limit");
    sp.set_subtitle("MB");
    ex.add_row(&sp);
    let save = {
        let (me, d, ex, sp) = (Rc::downgrade(s), d.to_string(), ex.clone(), sp.clone());
        Rc::new(move || {
            let on = ex.enables_expansion();
            ex.set_subtitle(&esc(&sub(on)));
            if let (Err(e), Some(s)) = (vm_set_mem(&d, on.then(|| sp.value() as u64)), me.upgrade()) {
                s.toast(&format!("Could not save: {e}"));
            }
        })
    };
    let f = save.clone();
    ex.connect_enable_expansion_notify(move |_| f());
    sp.connect_value_notify(move |_| save());
    ex
}

pub fn vm_page(s: &S, d: &str) -> adw::NavigationPage {
    let path = format!("{VMS}/{d}");
    // now: state and the actions
    let g_now = group("", "");
    let r_state = row("…", "", Some(if vm_conf(d)["kind"] == "android" || d == "pixelos" { "phone-symbolic" } else { "computer-symbolic" }), false);
    g_now.add(&r_state);
    let acts = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).build();
    r_state.add_suffix(&acts);

    let hw = Rows::new(group("Hardware", ""));
    let open: Rc<RefCell<HashSet<String>>> = Rc::default();

    // performance: this VM's CPU, memory, GPU (every 2 s)
    let g_perf = group("Performance", "");
    let (r_cpu, r_mem, r_gpu) = (row("CPU", "…", None, false), row("Memory", "…", None, false), row("GPU", "…", None, false));
    for r in [&r_cpu, &r_mem, &r_gpu] {
        g_perf.add(r);
    }

    let g_res = group("Resources", "");
    g_res.add(&memory_row(s, d));

    let upd_rows = Rows::new(group("", ""));
    upd_rows.group.set_visible(false);

    let title = vm_name(d);
    let p = page(&title, &[&g_now, &hw.group, &g_perf, &g_res, &upd_rows.group], None);

    let refresh = {
        let (me, d, path, r_state, acts, hw, open) = (Rc::downgrade(s), d.to_string(), path.clone(), r_state.clone(), acts.clone(), hw.clone(), open.clone());
        move || {
            let Some(s) = me.upgrade() else { return };
            let v = s.vm_info(&d);
            let power: PowerState = s.broker.state_as(Topic::Power);
            let running = v.as_ref().is_some_and(|v| v.running);
            let asleep = running && power.participants.get(&d).is_some_and(|e| e.state == VoteState::Suspended);
            let busy = v.as_ref().and_then(|v| v.busy.clone());
            let shown = s.handover().holders.get("display") == Some(&path);
            r_state.set_title(&esc(match (&busy, running, asleep) {
                (Some(b), ..) => b.as_str(),
                (_, true, true) => "Asleep",
                (_, true, false) => "Running",
                _ => "Stopped",
            }));
            r_state.set_subtitle(if running && shown { "has the screen" } else { "" });
            while let Some(c) = acts.first_child() {
                acts.remove(&c);
            }
            let act = |label: &str, style: Option<&str>, f: fn(&S, &str)| {
                let (me, d) = (Rc::downgrade(&s), d.clone());
                button(label, style, move || {
                    if let Some(s) = me.upgrade() {
                        f(&s, &d);
                    }
                })
            };
            if busy.is_some() {
            } else if asleep {
                acts.append(&act("Wake", Some("suggested-action"), |s, d| s.vm_power(d, true)));
                acts.append(&act("Stop", Some("destructive-action"), Shell::stop_vm));
            } else if running {
                acts.append(&act("Sleep", None, |s, d| s.vm_power(d, false)));
                acts.append(&act("Show", Some("suggested-action"), Shell::show_vm));
                acts.append(&act("Stop", Some("destructive-action"), Shell::stop_vm));
            } else {
                acts.append(&act("Start", Some("suggested-action"), Shell::start_vm));
            }
            hardware(&s, &d, &hw, &open);
        }
    };
    refresh();
    s.follow(&p.page, &[Topic::Vms, Topic::Power, Topic::Handover, Topic::Handles], refresh);

    // performance samples
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let prev: Rc<Cell<Option<(f64, u64, u64)>>> = Rc::default();
    let sample = {
        let (me, d) = (Rc::downgrade(s), d.to_string());
        move || {
            let Some(s) = me.upgrade() else { return };
            let (d, prev, r_cpu, r_mem, r_gpu) = (d.clone(), prev.clone(), r_cpu.clone(), r_mem.clone(), r_gpu.clone());
            s.broker.get(Op::Resources, move |r: Result<Resources, String>| {
                let Ok(r) = r else { return };
                let Some(v) = r.vms.iter().find(|v| crate::base(&v.dir) == d) else {
                    prev.set(None);
                    for x in [&r_cpu, &r_mem, &r_gpu] {
                        x.set_subtitle("not running");
                    }
                    return;
                };
                r_mem.set_subtitle(&format!("{:.1} GB", v.pss_kb as f64 / 1048576.0));
                if let Some((t, ticks, gpu)) = prev.replace(Some((r.t, v.ticks, v.gpu_ns)))
                    && r.t > t
                {
                    let dt = r.t - t;
                    r_cpu.set_subtitle(&format!("{:.0} % of one core", 100.0 * v.ticks.saturating_sub(ticks) as f64 / tck / dt));
                    r_gpu.set_subtitle(&format!("{:.0} %", (100.0 * v.gpu_ns.saturating_sub(gpu) as f64 / (dt * 1e9)).min(100.0)));
                }
            });
        }
    };
    sample();
    s.live(&p.page, 2, sample);

    // an update from the store
    let (me, d2, upd) = (Rc::downgrade(s), d.to_string(), upd_rows.clone());
    s.broker.get(Op::StoreStatus, move |r: Result<StoreStatus, String>| {
        let (Ok(st), Some(s)) = (r, me.upgrade()) else { return };
        if !st.updates.contains(&d2) {
            return;
        }
        let ver = st.available.get(&d2).cloned().unwrap_or_default();
        let r = row("Update available", &if ver.is_empty() { "a new version".to_string() } else { ver }, Some("system-software-update-symbolic"), false);
        let (me, d3) = (Rc::downgrade(&s), d2.clone());
        r.add_suffix(&button("Update", Some("suggested-action"), move || {
            if let Some(s) = me.upgrade() {
                s.toast(&format!("Updating {}…", vm_name(&d3)));
                s.ask(Op::StoreInstall { vm: d3.clone() }, |_| {});
            }
        }));
        upd.add(&r);
        upd.group.set_visible(true);
    });
    p.page
}
