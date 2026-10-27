//! Kernel uevents (NETLINK_KOBJECT_UEVENT) and waiting on file descriptors: the device services follow the
//! hardware with these, the launcher the external display.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

/// Kernel uevents (NETLINK_KOBJECT_UEVENT, the kernel's multicast group), non-blocking.
pub fn uevents() -> std::io::Result<OwnedFd> {
    unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, libc::NETLINK_KOBJECT_UEVENT);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut sa: libc::sockaddr_nl = std::mem::zeroed();
        sa.nl_family = libc::AF_NETLINK as u16;
        sa.nl_groups = 1;
        if libc::bind(fd.as_raw_fd(), (&sa as *const libc::sockaddr_nl).cast(), std::mem::size_of::<libc::sockaddr_nl>() as u32) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}

/// The next pending uevent's text ("KEY=value" fields joined by NULs), if any.
pub fn read_uevent(fd: RawFd) -> Option<Vec<u8>> {
    let mut b = vec![0u8; 8192];
    let n = unsafe { libc::recv(fd, b.as_mut_ptr().cast(), b.len(), 0) };
    (n > 0).then(|| {
        b.truncate(n as usize);
        b
    })
}

/// Wait until one of `fds` is readable (or `timeout`); the readable ones.
pub fn poll(fds: &[RawFd], timeout: Option<Duration>) -> Vec<RawFd> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    let t = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
    let n = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, t) };
    if n <= 0 {
        return vec![];
    }
    p.iter().filter(|x| x.revents != 0).map(|x| x.fd).collect()
}
