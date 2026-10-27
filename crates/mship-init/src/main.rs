//! mship-init: /init of the phone's initramfs (the FIT u-boot boots at EL2; built static). As PID 1:
//!
//! 1. the usual mounts, then the dev link: a USB gadget "m" (NCM network 10.42.0.2/24, gateway 10.42.0.1, and an
//!    ACM serial shell) with telnet on :2323 (busybox: the shells of the RAM host)
//! 2. the persistent root: the partition named mship-root, mounted read-only, with a boot counter
//!    (/var/lib/mship-boot/attempts, reset by mship-boot-ok.service): after 3 unfinished boots, or with `mship.ram`
//!    on the command line, the phone stays in this RAM host. Otherwise switch_root to systemd.
//! 3. the RAM host: a root shell on the serial console (uart12), after a KVM report
//!
//! Not as PID 1 (the RAM host's diagnostics, formerly nio-diag): `mship-init kvm` (KVM API and VM creation),
//! `mship-init reboot [MODE]` (sync and restart; MODE, e.g. bootloader, goes to reboot-mode).

use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

const REVISION: &str = "6-rust";
const GADGET: &str = "/sys/kernel/config/usb_gadget/m";
const NEWROOT: &str = "/newroot";

fn log(msg: &str) {
    if let Ok(mut k) = fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = writeln!(k, "mothership-nio: {msg}");
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn mount(src: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: Option<&str>) -> std::io::Result<()> {
    let data = data.map(cstr);
    let r = unsafe {
        libc::mount(cstr(src).as_ptr(), cstr(target).as_ptr(), cstr(fstype).as_ptr(), flags, data.as_ref().map_or(std::ptr::null(), |d| d.as_ptr().cast()))
    };
    if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn write(path: impl AsRef<Path>, value: &str) -> std::io::Result<()> {
    fs::write(path, value)
}

// ---------------------------------------------------------------- network (ioctls: no ip tool needed)

fn ifreq(name: &str) -> libc::ifreq {
    let mut r: libc::ifreq = unsafe { std::mem::zeroed() };
    for (d, s) in r.ifr_name.iter_mut().zip(name.bytes()) {
        *d = s as libc::c_char;
    }
    r
}

fn sockaddr_in(ip: [u8; 4]) -> libc::sockaddr {
    let a = libc::sockaddr_in { sin_family: libc::AF_INET as u16, sin_port: 0, sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(ip) }, sin_zero: [0; 8] };
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(a) }
}

fn link_up(s: i32, name: &str) {
    let mut r = ifreq(name);
    unsafe {
        if libc::ioctl(s, libc::SIOCGIFFLAGS, &mut r) == 0 {
            r.ifr_ifru.ifru_flags |= libc::IFF_UP as i16;
            libc::ioctl(s, libc::SIOCSIFFLAGS, &r);
        }
    }
}

fn dev_link_network() {
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if s < 0 {
        return;
    }
    link_up(s, "lo");
    link_up(s, "usb0");
    let mut r = ifreq("usb0");
    r.ifr_ifru.ifru_addr = sockaddr_in([10, 42, 0, 2]);
    unsafe { libc::ioctl(s, libc::SIOCSIFADDR, &r) };
    r.ifr_ifru.ifru_netmask = sockaddr_in([255, 255, 255, 0]);
    unsafe { libc::ioctl(s, libc::SIOCSIFNETMASK, &r) };
    let mut rt: libc::rtentry = unsafe { std::mem::zeroed() };
    rt.rt_dst = sockaddr_in([0, 0, 0, 0]);
    rt.rt_genmask = sockaddr_in([0, 0, 0, 0]);
    rt.rt_gateway = sockaddr_in([10, 42, 0, 1]);
    rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    unsafe {
        libc::ioctl(s, libc::SIOCADDRT, &rt);
        libc::close(s);
    }
}

// ---------------------------------------------------------------- the dev link

/// The USB gadget (NCM + ACM), its network, telnet and the ACM shell: the pids of the helpers.
fn dev_link() -> Vec<i32> {
    let _ = mount("configfs", "/sys/kernel/config", "configfs", 0, None);
    let mut udc = None;
    for _ in 0..40 {
        udc = fs::read_dir("/sys/class/udc").ok().and_then(|mut d| d.next()).and_then(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned());
        if udc.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let Some(udc) = udc else {
        log("USB_UDC_MISSING");
        return vec![];
    };
    let g = Path::new(GADGET);
    for d in ["strings/0x409", "configs/c.1/strings/0x409", "functions/ncm.usb0", "functions/acm.usb0"] {
        let _ = fs::create_dir_all(g.join(d));
    }
    let settings = [
        ("idVendor", "0x1d6b"),
        ("idProduct", "0x0104"),
        ("strings/0x409/manufacturer", "mothership"),
        ("strings/0x409/product", "nio-el2"),
        ("functions/ncm.usb0/host_addr", "ae:65:c1:c0:ad:eb"),
        ("functions/ncm.usb0/dev_addr", "12:1d:5a:79:eb:49"),
        ("configs/c.1/strings/0x409/configuration", "c"),
    ];
    for (f, v) in settings {
        let _ = write(g.join(f), v);
    }
    for f in ["ncm.usb0", "acm.usb0"] {
        let _ = std::os::unix::fs::symlink(g.join("functions").join(f), g.join("configs/c.1").join(f));
    }
    if write(g.join("UDC"), &udc).is_ok() {
        log(&format!("USB_GADGET_BOUND udc={udc}"));
    }
    std::thread::sleep(Duration::from_secs(1));
    dev_link_network();
    let mut helpers = vec![];
    if let Ok(c) = Command::new("/bin/busybox").args(["telnetd", "-F", "-l", "/bin/sh", "-p", "2323"]).spawn() {
        helpers.push(c.id() as i32);
    }
    // a shell on the ACM serial port, again whenever it ends
    match unsafe { libc::fork() } {
        0 => loop {
            if Path::new("/dev/ttyGS0").exists() {
                let _ = shell_on("/dev/ttyGS0").and_then(|mut c| c.wait());
            }
            std::thread::sleep(Duration::from_secs(1));
        },
        pid if pid > 0 => helpers.push(pid),
        _ => {}
    }
    helpers
}

/// An interactive busybox shell with `tty` as its controlling terminal.
fn shell_on(tty: &str) -> std::io::Result<std::process::Child> {
    let f = fs::OpenOptions::new().read(true).write(true).open(tty)?;
    let (i, o, e) = (f.try_clone()?, f.try_clone()?, f);
    let mut c = Command::new("/bin/sh");
    c.arg("-i").stdin(i).stdout(o).stderr(e);
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 0);
            Ok(())
        });
    }
    c.spawn()
}

// ---------------------------------------------------------------- the persistent root

fn find_partition(name: &str) -> Option<String> {
    fs::read_dir("/sys/class/block").ok()?.flatten().find_map(|e| {
        let u = fs::read_to_string(e.path().join("uevent")).ok()?;
        u.lines().any(|l| l == format!("PARTNAME={name}")).then(|| e.file_name().to_string_lossy().into_owned())
    })
}

/// Remove the initramfs's files (they would hold RAM): everything on the root device, not mounts below it.
fn free_initramfs(dir: &Path, dev: u64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if m.dev() != dev {
            continue;
        }
        if m.is_dir() {
            free_initramfs(&p, dev);
            let _ = fs::remove_dir(&p);
        } else {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Switch to mship-root unless disabled or failing; returns only if the phone stays in the RAM host.
fn try_root(helpers: &[i32]) {
    if fs::read_to_string("/proc/cmdline").is_ok_and(|c| c.split_whitespace().any(|w| w == "mship.ram")) {
        return log("ROOT_SKIP cmdline");
    }
    let Some(d) = find_partition("mship-root") else { return log("ROOT_NOT_FOUND") };
    let _ = fs::create_dir_all(NEWROOT);
    if let Err(e) = mount(&format!("/dev/{d}"), NEWROOT, "ext4", 0, None) {
        return log(&format!("ROOT_MOUNT_FAIL /dev/{d} ({e})"));
    }
    let undo = || unsafe { libc::umount(cstr(NEWROOT).as_ptr()) };
    if !Path::new(NEWROOT).join("usr/lib/systemd/systemd").exists() {
        undo();
        return log("ROOT_NO_INIT");
    }
    let attempts = Path::new(NEWROOT).join("var/lib/mship-boot/attempts");
    let n: u32 = fs::read_to_string(&attempts).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if n >= 3 {
        let _ = write(&attempts, "0\n");
        unsafe { libc::sync() };
        undo();
        return log(&format!("ROOT_FALLBACK attempts={n} -> staying in RAM host"));
    }
    let _ = fs::create_dir_all(attempts.parent().unwrap());
    let _ = write(&attempts, &format!("{}\n", n + 1));
    unsafe { libc::sync() };
    let _ = mount("none", NEWROOT, "", libc::MS_REMOUNT | libc::MS_RDONLY, None);
    log(&format!("ROOT_SWITCH /dev/{d} attempt={}", n + 1));
    // the USB serial and the telnet port go to the new root's services
    for p in helpers {
        unsafe { libc::kill(*p, libc::SIGKILL) };
    }
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        if let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>()
            && pid != 1
            && fs::read_link(e.path().join("fd/0")).is_ok_and(|l| l.to_string_lossy().contains("ttyGS0"))
        {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    unsafe { libc::umount(cstr("/tmp").as_ptr()) };
    for m in ["dev", "proc", "sys"] {
        let _ = mount(&format!("/{m}"), &format!("{NEWROOT}/{m}"), "", libc::MS_MOVE, None);
    }
    // switch_root: drop the initramfs, make the new root /, start systemd
    let dev = fs::metadata("/").map(|m| m.dev()).unwrap_or(0);
    free_initramfs(Path::new("/"), dev);
    unsafe {
        libc::chdir(cstr(NEWROOT).as_ptr());
        libc::mount(cstr(".").as_ptr(), cstr("/").as_ptr(), std::ptr::null(), libc::MS_MOVE, std::ptr::null());
        libc::chroot(cstr(".").as_ptr());
        libc::chdir(cstr("/").as_ptr());
    }
    let e = Command::new("/usr/lib/systemd/systemd").exec();
    log(&format!("ROOT_EXEC_FAIL ({e})"));
}

// ---------------------------------------------------------------- the RAM host

/// The kernel log (what `dmesg` shows)
fn kernel_log() -> String {
    let n = unsafe { libc::klogctl(10, std::ptr::null_mut(), 0) }; // SYSLOG_ACTION_SIZE_BUFFER
    let mut buf = vec![0u8; n.max(1 << 16) as usize];
    let got = unsafe { libc::klogctl(3, buf.as_mut_ptr().cast(), buf.len() as i32) }; // SYSLOG_ACTION_READ_ALL
    String::from_utf8_lossy(&buf[..got.max(0) as usize]).into_owned()
}

fn ram_host() -> ! {
    if let Ok(c) = fs::OpenOptions::new().read(true).write(true).open("/dev/console") {
        use std::os::fd::AsRawFd;
        for fd in 0..3 {
            unsafe { libc::dup2(c.as_raw_fd(), fd) };
        }
    }
    let klog = kernel_log();
    let el: Vec<&str> = klog.lines().filter(|l| { let l = l.to_lowercase(); l.contains("started at el") || l.contains("hyp mode") || l.contains("kvm [") }).take(3).collect();
    let kvm: Vec<&str> = klog.lines().filter(|l| l.to_lowercase().contains("kvm")).take(5).collect();
    println!("\n==================================================================");
    println!(" mothershipOS nio -- EL2 RAM host (serial console / uart12)");
    println!("------------------------------------------------------------------");
    println!(" CurrentEL (from kernel): {}", el.join(" | "));
    println!(" /dev/kvm: {}", if Path::new("/dev/kvm").exists() { "present" } else { "ABSENT" });
    println!(" KVM dmesg: {}", kvm.join(" | "));
    kvm_probe();
    println!("==================================================================");
    println!(" Dropping to root shell on the serial console.\n");
    loop {
        let _ = shell_on("/dev/console").and_then(|mut c| c.wait());
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn init() -> ! {
    for d in ["/proc", "/sys", "/dev", "/tmp", "/run", "/sbin", "/usr/bin", "/usr/sbin"] {
        let _ = fs::create_dir_all(d);
    }
    // busybox's applets for the RAM host's shells
    let _ = Command::new("/bin/busybox").args(["--install", "-s"]).status();
    let _ = mount("proc", "/proc", "proc", 0, None);
    let _ = mount("sysfs", "/sys", "sysfs", 0, None);
    let _ = mount("devtmpfs", "/dev", "devtmpfs", 0, None);
    let _ = mount("tmpfs", "/tmp", "tmpfs", 0, None);
    let _ = fs::create_dir_all("/dev/pts");
    let _ = mount("devpts", "/dev/pts", "devpts", 0, None);
    let _ = mount("pstore", "/sys/fs/pstore", "pstore", 0, None);
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    log(&format!("INIT_REACHED revision={REVISION} kernel={}", release.trim()));
    let helpers = dev_link();
    let _ = fs::create_dir_all("/sys/fs/cgroup");
    let _ = mount("none", "/sys/fs/cgroup", "cgroup2", 0, None);
    try_root(&helpers);
    ram_host()
}

// ---------------------------------------------------------------- diagnostics (formerly nio-diag)

const KVM_GET_API_VERSION: libc::c_ulong = 0xAE00;
const KVM_CREATE_VM: libc::c_ulong = 0xAE01;
const KVM_API_VERSION: i32 = 12;

fn kvm_probe() -> u8 {
    let kvm = unsafe { libc::open(cstr("/dev/kvm").as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if kvm < 0 {
        println!("KVM_OPEN_FAILED ({})", std::io::Error::last_os_error());
        return 1;
    }
    let version = unsafe { libc::ioctl(kvm, KVM_GET_API_VERSION, 0) };
    println!("KVM_API_VERSION={version} expected={KVM_API_VERSION}");
    if version != KVM_API_VERSION {
        return 1;
    }
    let vm = unsafe { libc::ioctl(kvm, KVM_CREATE_VM, 0) };
    if vm < 0 {
        println!("KVM_CREATE_VM_FAILED ({})", std::io::Error::last_os_error());
        return 1;
    }
    println!("KVM_CREATE_VM_OK; guest execution remains untested");
    unsafe {
        libc::close(vm);
        libc::close(kvm);
    }
    0
}

fn restart(mode: Option<&str>) -> u8 {
    unsafe { libc::sync() };
    let r = match mode {
        Some(m) => unsafe {
            libc::syscall(libc::SYS_reboot, libc::LINUX_REBOOT_MAGIC1, libc::LINUX_REBOOT_MAGIC2, libc::LINUX_REBOOT_CMD_RESTART2, cstr(m).as_ptr())
        },
        None => (unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_RESTART) }) as libc::c_long,
    };
    println!("REBOOT_FAILED ({}, {r})", std::io::Error::last_os_error());
    1
}

fn main() -> ExitCode {
    if std::process::id() == 1 {
        init();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["kvm"] => kvm_probe(),
        ["reboot"] => restart(None),
        ["reboot", mode] => restart(Some(mode)),
        _ => {
            eprintln!("usage: mship-init kvm | reboot [MODE]   (as PID 1: the initramfs init)");
            2
        }
    })
}
