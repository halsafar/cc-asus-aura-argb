// SPDX-License-Identifier: GPL-3.0-or-later
//
// hidraw access. `HidBus` is the seam between the pure protocol code and the
// hardware; tests use the mock below.
//
// The CoolerControl daemon's liquidctl helper may hold the same hidraw node open.
// hidraw hands every input report to every open file, so this plugin can see replies
// to the other process's requests. It only reads at startup (firmware and config
// table), matches replies by id, retries a bounded number of times, and never depends
// on a read afterwards.

use crate::protocol::{self, REPORT_LEN, Report};
use log::debug;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// One HID interface: whole 65-byte reports in and out.
pub trait HidBus {
    /// Writes one output report (report id first).
    fn write(&mut self, report: &Report) -> io::Result<()>;
    /// Waits up to `timeout` for one input report. `Ok(None)` on timeout.
    fn read(&mut self, timeout: Duration) -> io::Result<Option<Report>>;
    /// Throws away input reports that are already queued. Returns how many.
    fn drain(&mut self) -> io::Result<usize>;
    /// Opens the device again after it went away (e.g. re-enumerated after resume).
    fn reopen(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this bus cannot be reopened",
        ))
    }
}

impl<T: HidBus + ?Sized> HidBus for Box<T> {
    fn write(&mut self, report: &Report) -> io::Result<()> {
        (**self).write(report)
    }
    fn read(&mut self, timeout: Duration) -> io::Result<Option<Report>> {
        (**self).read(timeout)
    }
    fn drain(&mut self) -> io::Result<usize> {
        (**self).drain()
    }
    fn reopen(&mut self) -> io::Result<()> {
        (**self).reopen()
    }
}

// From <linux/hidraw.h>: struct hidraw_devinfo { __u32 bustype; __s16 vendor; __s16 product; }
#[repr(C)]
#[derive(Default)]
struct HidrawDevinfo {
    bustype: u32,
    vendor: i16,
    product: i16,
}
// _IOR('H', 0x03, struct hidraw_devinfo)
const HIDIOCGRAWINFO: u64 =
    (2 << 30) | ((std::mem::size_of::<HidrawDevinfo>() as u64) << 16) | (0x48 << 8) | 0x03;

/// Re-finds the node after the device went away: returns the new `/dev/hidrawN`.
pub type Locator = Box<dyn Fn() -> io::Result<PathBuf> + Send>;

/// A `/dev/hidrawN` node, opened non-blocking; reads wait with `poll`.
pub struct LinuxHidraw {
    file: File,
    path: PathBuf,
    ids: (u16, u16),
    locate: Option<Locator>,
}

impl LinuxHidraw {
    /// Opens `path` and checks with `HIDIOCGRAWINFO` (answered by the kernel, no USB
    /// traffic) that it is the expected USB device.
    pub fn open(path: &Path, vendor: u16, product: u16) -> io::Result<Self> {
        let file = open_checked(path, vendor, product)?;
        Ok(LinuxHidraw {
            file,
            path: path.to_path_buf(),
            ids: (vendor, product),
            locate: None,
        })
    }

    /// Sets how to find the node again for [`HidBus::reopen`].
    pub fn with_locator(mut self, locate: Locator) -> Self {
        self.locate = Some(locate);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_checked(path: &Path, vendor: u16, product: u16) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let mut info = HidrawDevinfo::default();
    // SAFETY: HIDIOCGRAWINFO writes one `struct hidraw_devinfo`, whose layout
    // `HidrawDevinfo` matches, into the pointed-to value, which lives across the call.
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            HIDIOCGRAWINFO as _,
            &mut info as *mut HidrawDevinfo,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let got = (info.vendor as u16, info.product as u16);
    if got != (vendor, product) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is {:04x}:{:04x}, expected {vendor:04x}:{product:04x}",
                path.display(),
                got.0,
                got.1
            ),
        ));
    }
    Ok(file)
}

impl HidBus for LinuxHidraw {
    fn write(&mut self, report: &Report) -> io::Result<()> {
        debug!(
            "hid write {}: {}",
            self.path.display(),
            protocol::hex_trimmed(report)
        );
        let n = self.file.write(report)?;
        if n != REPORT_LEN {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short write: {n} of {REPORT_LEN} bytes"),
            ));
        }
        Ok(())
    }

    fn read(&mut self, timeout: Duration) -> io::Result<Option<Report>> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = left.as_millis().min(i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd, count 1.
            let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
            if ret < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if ret == 0 {
                return Ok(None);
            }
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
                && pfd.revents & libc::POLLIN == 0
            {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            let mut buf = [0u8; REPORT_LEN];
            match self.file.read(&mut buf) {
                Ok(n) => {
                    debug!(
                        "hid read  {}: {}",
                        self.path.display(),
                        protocol::hex_trimmed(&buf[..n])
                    );
                    if n == 0 {
                        return Err(io::Error::from_raw_os_error(libc::ENODEV));
                    }
                    return Ok(Some(buf));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if left.is_zero() {
                        return Ok(None);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn drain(&mut self) -> io::Result<usize> {
        let mut buf = [0u8; REPORT_LEN];
        // hidraw keeps at most 64 reports per open file.
        for n in 0..256 {
            match self.file.read(&mut buf) {
                Ok(0) => return Err(io::Error::from_raw_os_error(libc::ENODEV)),
                Ok(_) => debug!(
                    "hid drain {}: {}",
                    self.path.display(),
                    protocol::hex_trimmed(&buf)
                ),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(n),
                Err(e) => return Err(e),
            }
        }
        Ok(256)
    }

    fn reopen(&mut self) -> io::Result<()> {
        let Some(locate) = &self.locate else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no way to find the device again",
            ));
        };
        let path = locate()?;
        let file = open_checked(&path, self.ids.0, self.ids.1)?;
        log::info!(
            "Reopened the controller: {} (was {})",
            path.display(),
            self.path.display()
        );
        self.file = file;
        self.path = path;
        Ok(())
    }
}

/// Whether an I/O error means the device node is gone and reopening may help.
pub fn is_gone(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENODEV) | Some(libc::ENXIO) | Some(libc::ESHUTDOWN) | Some(libc::EPIPE)
    )
}

#[derive(Debug)]
pub enum QueryError {
    Io(io::Error),
    /// No reply with the expected id within all attempts.
    NoReply {
        reply_id: u8,
        attempts: u32,
        /// Reports seen that were not the expected reply (other processes' traffic).
        other: usize,
    },
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Io(e) => write!(f, "HID I/O failed: {e}"),
            QueryError::NoReply {
                reply_id,
                attempts,
                other,
            } => write!(
                f,
                "no reply 0x{reply_id:02x} after {attempts} attempt(s) ({other} unrelated \
                 report(s) seen)"
            ),
        }
    }
}

impl std::error::Error for QueryError {}

/// How startup requests are retried.
#[derive(Clone, Copy, Debug)]
pub struct QueryPolicy {
    pub attempts: u32,
    /// How long to wait for the reply after each request.
    pub timeout: Duration,
}

impl Default for QueryPolicy {
    fn default() -> Self {
        QueryPolicy {
            attempts: 3,
            timeout: Duration::from_millis(500),
        }
    }
}

/// Upper bound on unrelated reports read while waiting for one reply.
const MAX_SKIPPED_PER_ATTEMPT: usize = 64;

/// Sends `request` and returns the first report whose id is `reply_id`. Before each
/// attempt queued input is drained; reports with other ids (replies to another
/// process) are skipped. Gives up after `policy.attempts`.
pub fn query<B: HidBus + ?Sized>(
    bus: &mut B,
    request: &Report,
    reply_id: u8,
    policy: QueryPolicy,
) -> Result<Report, QueryError> {
    let mut other = 0;
    for attempt in 1..=policy.attempts {
        let stale = bus.drain().map_err(QueryError::Io)?;
        if stale > 0 {
            debug!("discarded {stale} queued input report(s)");
        }
        bus.write(request).map_err(QueryError::Io)?;
        let deadline = Instant::now() + policy.timeout;
        // Bounded even if another process keeps the input queue full.
        for _ in 0..MAX_SKIPPED_PER_ATTEMPT {
            let left = deadline.saturating_duration_since(Instant::now());
            match bus.read(left).map_err(QueryError::Io)? {
                Some(r) if protocol::check_reply(&r, reply_id).is_ok() => return Ok(r),
                Some(r) => {
                    other += 1;
                    debug!(
                        "attempt {attempt}: skipping report {} (waiting for 0x{reply_id:02x})",
                        protocol::hex_trimmed(&r[..8])
                    );
                }
                None => break,
            }
        }
        debug!("attempt {attempt}: no reply 0x{reply_id:02x}");
    }
    Err(QueryError::NoReply {
        reply_id,
        attempts: policy.attempts,
        other,
    })
}

/// Test double: records every write, answers requests with scripted replies.
#[doc(hidden)]
pub mod mock {
    use super::HidBus;
    use crate::protocol::{REPORT_LEN, Report};
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Op {
        Write(Report),
        Read,
        Drain(usize),
        Reopen,
    }

    #[derive(Default)]
    struct Inner {
        ops: Vec<Op>,
        input: VecDeque<Report>,
        /// (request prefix, batches of replies; each matching write queues one batch)
        responders: Vec<(Vec<u8>, VecDeque<Vec<Report>>)>,
        fail_writes: Option<i32>,
        /// Fail this many writes with ENODEV, then succeed.
        gone_for: usize,
        reopen_fails: bool,
    }

    /// Cloneable handle; all clones share the same state.
    #[derive(Clone, Default)]
    pub struct MockBus(Arc<Mutex<Inner>>);

    pub fn report(lead: &[u8]) -> Report {
        let mut r = [0u8; REPORT_LEN];
        r[..lead.len()].copy_from_slice(lead);
        r
    }

    impl MockBus {
        pub fn new() -> Self {
            Self::default()
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
            self.0.lock().unwrap()
        }

        /// The next write starting with `prefix` queues `replies` as input.
        pub fn respond(&self, prefix: &[u8], replies: Vec<Report>) {
            let mut i = self.lock();
            if let Some((_, q)) = i.responders.iter_mut().find(|(p, _)| p == prefix) {
                q.push_back(replies);
            } else {
                i.responders
                    .push((prefix.to_vec(), VecDeque::from([replies])));
            }
        }

        /// Queues an input report now (e.g. another process's reply).
        pub fn inject(&self, r: Report) {
            self.lock().input.push_back(r);
        }

        /// Every write fails with this errno (`None`: writes succeed).
        pub fn fail_writes(&self, errno: Option<i32>) {
            self.lock().fail_writes = errno;
        }

        /// The next `n` writes fail with ENODEV.
        pub fn gone_for(&self, n: usize) {
            self.lock().gone_for = n;
        }

        pub fn reopen_fails(&self, fail: bool) {
            self.lock().reopen_fails = fail;
        }

        pub fn ops(&self) -> Vec<Op> {
            self.lock().ops.clone()
        }

        pub fn writes(&self) -> Vec<Report> {
            self.ops()
                .into_iter()
                .filter_map(|op| match op {
                    Op::Write(r) => Some(r),
                    _ => None,
                })
                .collect()
        }

        /// Writes with their trailing zeros dropped, for readable assertions.
        pub fn writes_trimmed(&self) -> Vec<Vec<u8>> {
            self.writes()
                .iter()
                .map(|r| {
                    let end = r.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
                    r[..end].to_vec()
                })
                .collect()
        }

        pub fn clear(&self) {
            self.lock().ops.clear();
        }
    }

    impl HidBus for MockBus {
        fn write(&mut self, report: &Report) -> io::Result<()> {
            let mut i = self.lock();
            if i.gone_for > 0 {
                i.gone_for -= 1;
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            if let Some(errno) = i.fail_writes {
                return Err(io::Error::from_raw_os_error(errno));
            }
            i.ops.push(Op::Write(*report));
            let batch = i
                .responders
                .iter_mut()
                .find(|(p, _)| report.starts_with(p))
                .and_then(|(_, q)| q.pop_front());
            if let Some(b) = batch {
                i.input.extend(b);
            }
            Ok(())
        }

        fn read(&mut self, _timeout: Duration) -> io::Result<Option<Report>> {
            let mut i = self.lock();
            i.ops.push(Op::Read);
            Ok(i.input.pop_front())
        }

        fn drain(&mut self) -> io::Result<usize> {
            let mut i = self.lock();
            let n = i.input.len();
            i.input.clear();
            i.ops.push(Op::Drain(n));
            Ok(n)
        }

        fn reopen(&mut self) -> io::Result<()> {
            let mut i = self.lock();
            i.ops.push(Op::Reopen);
            if i.reopen_fails {
                Err(io::Error::from_raw_os_error(libc::ENOENT))
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::{MockBus, Op, report};
    use super::*;

    const FAST: QueryPolicy = QueryPolicy {
        attempts: 3,
        timeout: Duration::from_millis(1),
    };

    #[test]
    fn query_returns_matching_reply() {
        let mut bus = MockBus::new();
        let reply = report(&[0xEC, 0x02, b'A']);
        bus.respond(&[0xEC, 0x82], vec![reply]);
        let got = query(&mut bus, &protocol::firmware_request(), 0x02, FAST).unwrap();
        assert_eq!(got, reply);
        assert_eq!(
            bus.ops(),
            vec![
                Op::Drain(0),
                Op::Write(protocol::firmware_request()),
                Op::Read
            ]
        );
    }

    #[test]
    fn query_skips_other_processes_reports() {
        let mut bus = MockBus::new();
        // Stale input from before the request is drained.
        bus.inject(report(&[0xEC, 0x02, b'X']));
        // The other process's config reply arrives first, then ours.
        let ours = report(&[0xEC, 0x30, 0, 0, 1, 2, 3]);
        bus.respond(
            &[0xEC, 0xB0],
            vec![report(&[0xEC, 0x02]), report(&[0x01, 0x30]), ours],
        );
        let got = query(&mut bus, &protocol::config_request(), 0x30, FAST).unwrap();
        assert_eq!(got, ours);
        assert_eq!(bus.ops()[0], Op::Drain(1));
    }

    #[test]
    fn query_retries_then_gives_up() {
        let mut bus = MockBus::new();
        // First attempt: no answer. Second: answered.
        bus.respond(&[0xEC, 0x82], vec![]);
        bus.respond(&[0xEC, 0x82], vec![report(&[0xEC, 0x02])]);
        query(&mut bus, &protocol::firmware_request(), 0x02, FAST).unwrap();
        assert_eq!(bus.writes().len(), 2);

        let mut bus = MockBus::new();
        bus.respond(&[0xEC, 0x82], vec![report(&[0xEC, 0x30])]);
        match query(&mut bus, &protocol::firmware_request(), 0x02, FAST) {
            Err(QueryError::NoReply {
                reply_id: 0x02,
                attempts: 3,
                other: 1,
            }) => {}
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            bus.writes().len(),
            3,
            "bounded: exactly `attempts` requests"
        );

        let mut bus = MockBus::new();
        bus.fail_writes(Some(libc::EIO));
        assert!(matches!(
            query(&mut bus, &protocol::firmware_request(), 0x02, FAST),
            Err(QueryError::Io(_))
        ));
    }

    #[test]
    fn ioctl_number_and_struct() {
        assert_eq!(std::mem::size_of::<HidrawDevinfo>(), 8);
        assert_eq!(HIDIOCGRAWINFO, 0x8008_4803);
    }

    #[test]
    fn gone_errors() {
        assert!(is_gone(&io::Error::from_raw_os_error(libc::ENODEV)));
        assert!(!is_gone(&io::Error::from_raw_os_error(libc::EIO)));
    }
}
