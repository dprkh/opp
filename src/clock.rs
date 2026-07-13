use std::io;
use std::time::{Duration, SystemTime};

pub(crate) trait Clock: Send + Sync {
    fn monotonic(&self) -> u64;
    fn wall(&self) -> SystemTime;
}

#[derive(Debug, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn monotonic(&self) -> u64 {
        monotonic_now().unwrap_or(0)
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

pub(crate) fn add(base: u64, duration: Duration) -> u64 {
    base.saturating_add(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
}

pub(crate) fn remaining(now: u64, deadline: u64) -> Duration {
    Duration::from_nanos(deadline.saturating_sub(now))
}

fn monotonic_now() -> io::Result<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `value` points to writable storage for a `timespec`.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut value) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let seconds = u64::try_from(value.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(value.tv_nsec).unwrap_or(0);
    Ok(seconds.saturating_mul(1_000_000_000).saturating_add(nanos))
}
