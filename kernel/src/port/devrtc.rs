//! `#r`, the real-time clock: `rtc` reads the seconds since 1970-01-01
//! UTC, as Plan 9 gives them, and writing a number there sets the time.
//!
//! There is no clock driver yet. The time is what the firmware's clock
//! said just before the handoff (`BootInfo::boot_time`), counted on by
//! the counter; setting it changes only the kernel's idea of it.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::{DMDIR, OpenMode, QTDIR, QTFILE, Qid};

use crate::port::chan::Chan;
use crate::port::clock;
use crate::port::dev::{self, Dev, Entry};
use crate::port::error::{EBADARG, EBADSPEC, EPERM, Result};

const NS_PER_SEC: u64 = 1_000_000_000;

/// Nanoseconds since the epoch at counter time 0.
static EPOCH_AT_ZERO: AtomicU64 = AtomicU64::new(0);

/// Takes the firmware's time, `boot_time` seconds since the epoch (0 if
/// it was unknown).
pub(crate) fn init(boot_time: u64) {
    if boot_time == 0 {
        crate::println!("rtc: firmware clock unreadable; time starts at 1970");
    } else {
        crate::println!("rtc: {boot_time} s since 1970 by the firmware clock");
    }
    set(boot_time.saturating_mul(NS_PER_SEC));
}

fn set(ns: u64) {
    EPOCH_AT_ZERO.store(ns.saturating_sub(clock::now_ns()), Ordering::Relaxed);
}

/// Nanoseconds since the epoch.
pub(crate) fn nanoseconds() -> u64 {
    EPOCH_AT_ZERO.load(Ordering::Relaxed).saturating_add(clock::now_ns())
}

/// Seconds since the epoch.
pub(crate) fn seconds() -> u64 {
    nanoseconds() / NS_PER_SEC
}

pub(crate) struct Rtc;

pub(crate) static RTC: Rtc = Rtc;

const DIR: Qid = Qid {
    path: 0,
    vers: 0,
    ty: QTDIR,
};
const FILE: Qid = Qid {
    path: 1,
    vers: 0,
    ty: QTFILE,
};

fn dirgen(_: &Chan, i: usize) -> Option<Entry> {
    (i == 0).then(|| Entry::new("rtc", FILE, 0, 0o664))
}

impl Dev for Rtc {
    fn dc(&self) -> char {
        'r'
    }

    fn name(&self) -> &'static str {
        "rtc"
    }

    fn attach(&self, spec: &str) -> Result<Chan> {
        if !spec.is_empty() {
            return Err(EBADSPEC.into());
        }
        Ok(Chan::new(&RTC, 0, DIR, String::from("#r")))
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Chan> {
        dev::walk_gen(c, name, &dirgen)
    }

    fn stat(&self, c: &Chan) -> Result<Vec<u8>> {
        let e = if c.qid.is_dir() {
            Entry::new("#r", DIR, 0, DMDIR | 0o555)
        } else {
            Entry::new("rtc", FILE, 0, 0o664)
        };
        Ok(e.encode(self, c.devno))
    }

    fn open(&self, c: Chan, mode: OpenMode) -> Result<Arc<Chan>> {
        let perm = if c.qid.is_dir() { DMDIR | 0o555 } else { 0o664 };
        dev::open_checked(c, mode, perm)
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.is_dir() {
            return dev::dirread_gen(c, buf, offset, &dirgen);
        }
        let text = alloc::format!("{:>11} ", seconds());
        Ok(dev::read_bytes(text.as_bytes(), buf, offset))
    }

    fn write(&self, c: &Chan, buf: &[u8], _: u64) -> Result<usize> {
        if c.qid.is_dir() {
            return Err(EPERM.into());
        }
        let secs: u64 = core::str::from_utf8(buf)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or(EBADARG)?;
        set(secs.saturating_mul(NS_PER_SEC));
        Ok(buf.len())
    }
}
