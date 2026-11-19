//! The VM store: the store's VMs with a search, install and update (mothershipd store.*). Installed VMs show their
//! version and whether the store has another; those with an update come first.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use mship_core::api::{Op, StoreList, StoreStatus, StoreVm};

use crate::S;
use crate::ui::{Rows, button, esc, group, page, row};

struct Store {
    shell: Weak<crate::Shell>,
    loading: Cell<bool>,
    list: RefCell<StoreList>,
    status: RefCell<StoreStatus>,
    query: RefCell<String>,
    updates: Rows,
    rows: Rows,
    addr: adw::ActionRow,
}

fn size(b: u64) -> String {
    if b >= 1 << 30 { format!("{:.1} GB", b as f64 / (1u64 << 30) as f64) } else { format!("{} MB", (b >> 20).max(1)) }
}

fn matches(v: &StoreVm, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty() || [&v.name, &v.id, &v.summary, &v.description].iter().any(|t| t.to_lowercase().contains(&q))
}

/// The home screen's line for the store: updates, or what it is for.
pub fn summary(st: &StoreStatus) -> String {
    match st.updates.len() {
        0 => "find, install and update VMs".into(),
        1 => "1 update available".into(),
        n => format!("{n} updates available"),
    }
}

/// what the store can do with a VM now: (subtitle, button label and style, or None)
fn state(v: &StoreVm, st: &StoreStatus) -> (String, Option<(&'static str, Option<&'static str>)>) {
    let ver = if v.version.is_empty() { "a new version".to_string() } else { v.version.clone() };
    let about = [v.version.as_str(), &size(v.size), v.summary.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
    if let Some(j) = st.jobs.get(&v.id) {
        return match j.phase.as_str() {
            "downloading" if j.total > 0 => (format!("downloading {} % · {} of {}", j.done * 100 / j.total, size(j.done), size(j.total)), None),
            "downloading" => ("downloading…".into(), None),
            "failed" => (format!("failed: {}", j.detail), Some(("Retry", Some("suggested-action")))),
            _ => (format!("installing · {}", j.detail), None),
        };
    }
    match st.installed.get(&v.id) {
        Some(have) if *have == v.version => (format!("installed {have} · up to date"), None),
        Some(have) if have.is_empty() => (format!("installed (version unknown) · {ver} available · {}", size(v.size)), Some(("Update", Some("suggested-action")))),
        Some(have) => (format!("installed {have} · {ver} available · {}", size(v.size)), Some(("Update", Some("suggested-action")))),
        None => (about, Some(("Install", Some("suggested-action")))),
    }
}

fn icon(v: &StoreVm) -> gtk::Image {
    let img = match &v.icon {
        Some(f) => gtk::Image::from_file(f),
        None => gtk::Image::from_icon_name("application-x-addon-symbolic"),
    };
    img.set_pixel_size(48);
    img
}

fn install(s: &S, st: &Rc<Store>, v: &StoreVm) {
    s.toast(&format!("Installing {}…", v.name));
    let st = st.clone();
    s.ask_t(Op::StoreInstall { vm: v.id.clone() }, move |r: StoreStatus| {
        *st.status.borrow_mut() = r;
        redraw(&st);
    });
}

fn details(s: &S, st: &Rc<Store>, v: &StoreVm) {
    let (sub, act) = state(v, &st.status.borrow());
    let body = format!("{}\n\n{sub}", if v.description.is_empty() { &v.summary } else { &v.description });
    let dlg = adw::AlertDialog::new(Some(&v.name), Some(&body));
    dlg.add_response("close", "Close");
    if let Some((label, _)) = act {
        dlg.add_response("go", label);
        dlg.set_response_appearance("go", adw::ResponseAppearance::Suggested);
    }
    let (me, st2, v2) = (Rc::downgrade(s), st.clone(), v.clone());
    dlg.connect_response(None, move |_, resp| {
        if let (Some(s), "go") = (me.upgrade(), resp) {
            install(&s, &st2, &v2);
        }
    });
    dlg.present(Some(s.window()));
}

/// The rows again: the list, its search, the installs' state.
fn redraw(st: &Rc<Store>) {
    let Some(s) = st.shell.upgrade() else { return };
    if st.loading.get() {
        return;
    }
    st.rows.clear();
    st.updates.clear();
    st.updates.group.set_visible(false);
    let list = st.list.borrow();
    let status = st.status.borrow();
    st.rows.group.set_description(Some(&if status.checked > 0 {
        let t = gtk::glib::DateTime::from_unix_local(status.checked as i64).ok().and_then(|t| t.format("%H:%M").ok());
        format!("Checked for updates at {}", t.map(|t| t.to_string()).unwrap_or_default())
    } else {
        String::new()
    }));
    st.addr.set_subtitle(&esc(if list.url.is_empty() { "not set: tap to set the store's address" } else { &list.url }));
    if let Some(e) = &list.error {
        let r = row(if list.url.is_empty() { "No store set" } else { "Can't reach the store" }, e, Some("network-offline-symbolic"), false);
        st.rows.add(&r);
        return;
    }
    let q = st.query.borrow();
    let shown: Vec<&StoreVm> = list.vms.iter().filter(|v| matches(v, &q)).collect();
    if shown.is_empty() {
        st.rows.add(&row(if list.vms.is_empty() { "The store has no VMs" } else { "Nothing found" }, "", None, false));
    }
    for v in shown {
        let (sub, act) = state(v, &status);
        let update = status.updates.contains(&v.id) && !status.jobs.contains_key(&v.id);
        let r = adw::ActionRow::builder().title(esc(&v.name)).subtitle(esc(&sub)).subtitle_lines(2).activatable(true).build();
        r.add_prefix(&icon(v));
        if let Some((label, style)) = act {
            let (me, st2, v2) = (Rc::downgrade(&s), st.clone(), v.clone());
            r.add_suffix(&button(label, style, move || {
                if let Some(s) = me.upgrade() {
                    install(&s, &st2, &v2);
                }
            }));
        }
        let (me, st2, v2) = (Rc::downgrade(&s), st.clone(), v.clone());
        r.connect_activated(move |_| {
            if let Some(s) = me.upgrade() {
                details(&s, &st2, &v2);
            }
        });
        if update {
            st.updates.group.set_visible(true);
            st.updates.add(&r);
        } else {
            st.rows.add(&r);
        }
    }
}

fn load(s: &S, st: &Rc<Store>) {
    st.loading.set(true);
    st.rows.clear();
    st.rows.add(&row("Loading…", "", Some("content-loading-symbolic"), false));
    let st2 = st.clone();
    s.broker.get(Op::StoreList, move |r: Result<StoreList, String>| {
        st2.loading.set(false);
        *st2.list.borrow_mut() = r.unwrap_or_else(|e| StoreList { error: Some(e), ..Default::default() });
        redraw(&st2);
    });
}

fn address_dialog(s: &S, st: &Rc<Store>) {
    let dlg = adw::AlertDialog::new(Some("Store address"), Some("A vmstore server, e.g. https://vmstore.example.org. Empty: mothershipOS's own store."));
    let entry = gtk::Entry::builder().text(st.list.borrow().url.as_str()).input_purpose(gtk::InputPurpose::Url).build();
    dlg.set_extra_child(Some(&entry));
    dlg.add_response("cancel", "Cancel");
    dlg.add_response("save", "Save");
    dlg.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    let (me, st2) = (Rc::downgrade(s), st.clone());
    dlg.connect_response(None, move |_, resp| {
        let Some(s) = me.upgrade() else { return };
        if resp != "save" {
            return;
        }
        let (me, st3) = (Rc::downgrade(&s), st2.clone());
        s.ask_t(Op::StoreSet { url: entry.text().to_string() }, move |r: StoreStatus| {
            *st3.status.borrow_mut() = r;
            if let Some(s) = me.upgrade() {
                load(&s, &st3);
            }
        });
    });
    dlg.present(Some(s.window()));
}

pub fn store_page(s: &S) -> adw::NavigationPage {
    let search = gtk::SearchEntry::builder().placeholder_text("Search VMs").hexpand(true).build();
    let g_search = group("", "");
    g_search.add(&search);
    let updates = Rows::new(group("Updates", ""));
    updates.group.set_visible(false);
    let rows = Rows::new(group("VMs", ""));
    let g_addr = group("", "");
    let addr = row("Store", "…", Some("network-server-symbolic"), true);
    g_addr.add(&addr);
    let st = Rc::new(Store { shell: Rc::downgrade(s), loading: Cell::new(false), list: RefCell::default(), status: RefCell::default(), query: RefCell::default(), updates, rows, addr });
    let reload = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Reload").build();
    let p = page("VM store", &[&g_search, &st.updates.group, &st.rows.group, &g_addr], Some(reload.upcast_ref()));

    let st2 = st.clone();
    search.connect_search_changed(move |e| {
        *st2.query.borrow_mut() = e.text().to_string();
        redraw(&st2);
    });
    let (me, st2) = (Rc::downgrade(s), st.clone());
    reload.connect_clicked(move |_| {
        if let Some(s) = me.upgrade() {
            load(&s, &st2);
        }
    });
    let (me, st2) = (Rc::downgrade(s), st.clone());
    st.addr.connect_activated(move |_| {
        if let Some(s) = me.upgrade() {
            address_dialog(&s, &st2);
        }
    });
    load(s, &st);
    // installs in progress and what is installed: every 2 s while the page is open, redrawn when it changed
    let (me, st2) = (Rc::downgrade(s), st.clone());
    let poll = move || {
        let Some(s) = me.upgrade() else { return };
        let st3 = st2.clone();
        s.broker.get(Op::StoreStatus, move |r: Result<StoreStatus, String>| {
            if let Ok(r) = r
                && *st3.status.borrow() != r
            {
                *st3.status.borrow_mut() = r;
                redraw(&st3);
            }
        });
    };
    poll();
    s.live(&p.page, 2, poll);
    // no focus on the search until it is tapped: else the keyboard covers the list as the page opens
    let win = s.window().clone();
    p.page.connect_shown(move |_| gtk::prelude::GtkWindowExt::set_focus(&win, None::<&gtk::Widget>));
    p.page
}
