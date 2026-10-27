//! Small widget helpers: rows, buttons, pages, groups with changing rows, live graphs.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

pub fn esc(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

pub fn row(title: &str, subtitle: &str, icon: Option<&str>, activatable: bool) -> adw::ActionRow {
    let r = adw::ActionRow::builder().title(esc(title)).subtitle(esc(subtitle)).build();
    if let Some(i) = icon {
        r.add_prefix(&gtk::Image::from_icon_name(i));
    }
    if activatable {
        r.set_activatable(true);
        r.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    }
    r
}

pub fn button(label: &str, style: Option<&str>, f: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::builder().label(label).valign(gtk::Align::Center).build();
    if let Some(s) = style {
        b.add_css_class(s);
    }
    b.connect_clicked(move |_| f());
    b
}

pub fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::new();
    if !title.is_empty() {
        g.set_title(&esc(title));
    }
    if !description.is_empty() {
        g.set_description(Some(&esc(description)));
    }
    g
}

pub fn strings(items: &[&str]) -> gtk::StringList {
    gtk::StringList::new(items)
}

/// A NavigationPage with a header bar around a scrolled PreferencesPage.
pub struct Page {
    pub page: adw::NavigationPage,
    pub prefs: adw::PreferencesPage,
    pub toolbar: adw::ToolbarView,
}

pub fn page(title: &str, groups: &[&adw::PreferencesGroup], header_extra: Option<&gtk::Widget>) -> Page {
    let prefs = adw::PreferencesPage::new();
    for g in groups {
        prefs.add(*g);
    }
    let toolbar = adw::ToolbarView::new();
    let hb = adw::HeaderBar::builder().decoration_layout("").build(); // a phone shell: no window buttons
    if let Some(w) = header_extra {
        hb.pack_end(w);
    }
    toolbar.add_top_bar(&hb);
    toolbar.set_content(Some(&prefs));
    Page { page: adw::NavigationPage::new(&toolbar, title), prefs, toolbar }
}

/// A group whose rows are replaced as a whole.
#[derive(Clone)]
pub struct Rows {
    pub group: adw::PreferencesGroup,
    rows: Rc<RefCell<Vec<gtk::Widget>>>,
}

impl Rows {
    pub fn new(group: adw::PreferencesGroup) -> Self {
        Rows { group, rows: Rc::default() }
    }

    pub fn clear(&self) {
        for r in self.rows.borrow_mut().drain(..) {
            self.group.remove(&r);
        }
    }

    pub fn add(&self, w: &impl IsA<gtk::Widget>) {
        self.group.add(w);
        self.rows.borrow_mut().push(w.clone().upcast());
    }
}

/// A filled line graph of the last n samples; scale is `top` or the visible maximum.
#[derive(Clone)]
pub struct Spark {
    pub area: gtk::DrawingArea,
    v: Rc<RefCell<Vec<f64>>>,
}

const SAMPLES: usize = 60;

impl Spark {
    pub fn new(color: (f64, f64, f64), top: Option<f64>) -> Self {
        let area = gtk::DrawingArea::builder().content_height(44).hexpand(true).margin_top(4).build();
        let v: Rc<RefCell<Vec<f64>>> = Rc::default();
        let vals = v.clone();
        area.set_draw_func(move |_, cr, w, h| {
            let v = vals.borrow();
            if v.len() < 2 {
                return;
            }
            let (w, h) = (w as f64, h as f64);
            let top = top.unwrap_or_else(|| (v.iter().cloned().fold(0.0, f64::max) * 1.15).max(1e-9));
            let step = w / (SAMPLES - 1) as f64;
            let x0 = w - step * (v.len() - 1) as f64;
            let pts: Vec<(f64, f64)> = v.iter().enumerate().map(|(i, x)| (x0 + i as f64 * step, h - 1.0 - (h - 3.0) * (x / top).min(1.0))).collect();
            let (r, g, b) = color;
            cr.move_to(pts[0].0, h);
            for p in &pts {
                cr.line_to(p.0, p.1);
            }
            cr.line_to(pts[pts.len() - 1].0, h);
            cr.close_path();
            cr.set_source_rgba(r, g, b, 0.22);
            let _ = cr.fill();
            cr.move_to(pts[0].0, pts[0].1);
            for p in &pts[1..] {
                cr.line_to(p.0, p.1);
            }
            cr.set_source_rgba(r, g, b, 1.0);
            cr.set_line_width(1.6);
            let _ = cr.stroke();
        });
        Spark { area, v }
    }

    pub fn push(&self, x: f64) {
        let mut v = self.v.borrow_mut();
        v.push(x);
        if v.len() > SAMPLES {
            v.remove(0);
        }
        drop(v);
        self.area.queue_draw();
    }
}

/// A row with a title, a live value and a graph.
#[derive(Clone)]
pub struct Metric {
    pub row: adw::PreferencesRow,
    value: gtk::Label,
    sub: gtk::Label,
    spark: Spark,
}

impl Metric {
    pub fn new(title: &str, color: (f64, f64, f64), top: Option<f64>) -> Self {
        let bx = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_start(12).margin_end(12).margin_top(10).margin_bottom(10).build();
        let head = gtk::Box::builder().spacing(8).build();
        head.append(&gtk::Label::builder().label(title).xalign(0.0).hexpand(true).css_classes(["heading"]).build());
        let value = gtk::Label::builder().label("…").xalign(1.0).css_classes(["dim"]).build();
        head.append(&value);
        let sub = gtk::Label::builder().label("").xalign(0.0).css_classes(["dim", "caption"]).wrap(true).build();
        let spark = Spark::new(color, top);
        bx.append(&head);
        bx.append(&sub);
        bx.append(&spark.area);
        let row = adw::PreferencesRow::builder().activatable(false).child(&bx).build();
        Metric { row, value, sub, spark }
    }

    pub fn set(&self, value: &str, sample: f64, sub: Option<&str>) {
        self.value.set_label(value);
        if let Some(s) = sub {
            self.sub.set_label(s);
        }
        self.spark.push(sample);
    }
}

pub fn mhz(khz: u64) -> String {
    if khz >= 1_000_000 { format!("{:.2} GHz", khz as f64 / 1e6) } else { format!("{} MHz", khz / 1000) }
}

pub fn fmt_rate(mut b: f64) -> String {
    for unit in ["B", "KB", "MB", "GB"] {
        if b < 1000.0 || unit == "GB" {
            return if unit == "B" { format!("{b:.0} {unit}/s") } else { format!("{b:.1} {unit}/s") };
        }
        b /= 1000.0;
    }
    unreachable!()
}

/// Local time as strftime formats it, following /etc/localtime as it is now (Settings › Region & time).
pub fn local_time(fmt: &str) -> String {
    unsafe extern "C" {
        fn tzset();
    }
    unsafe {
        tzset();
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        let mut buf = [0u8; 128];
        let f = std::ffi::CString::new(fmt).unwrap();
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), f.as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

/// Signal the session user's processes named `name`.
pub fn signal_named(name: &str, sig: i32) {
    let me = unsafe { libc::getuid() };
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|p| p.parse::<i32>().ok()) else { continue };
        use std::os::unix::fs::MetadataExt;
        if e.metadata().is_ok_and(|m| m.uid() == me) && std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == name) {
            unsafe { libc::kill(pid, sig) };
        }
    }
}

/// Start a program detached from the shell.
pub fn spawn(cmd: &str) {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new(cmd);
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().map(|mut ch| {
        std::thread::spawn(move || ch.wait());
    });
}
