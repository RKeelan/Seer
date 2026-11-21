//! Start-up timing: the process's creation time, the first `Present`, and the
//! phases between, reported by `seer debug timings`.
//!
//! Each phase is measured in milliseconds from the moment Windows created the
//! process, so the time spent loading the executable and its libraries before
//! `main` counts too.

use std::time::Instant;

use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

use crate::protocol::{Phase, Timings as Report};

/// Points in start-up, recorded as they are reached.
#[derive(Clone, Debug)]
pub struct Timings {
    origin: Instant,
    /// Milliseconds from the process's creation to `origin`.
    origin_ms: f64,
    phases: Vec<(&'static str, Instant)>,
}

impl Timings {
    /// Starts timing, reading when the process was created.
    pub fn start() -> Self {
        let origin = Instant::now();
        // SAFETY: the pseudo-handle for the current process is always valid.
        let origin_ms = process_age_ms(unsafe { GetCurrentProcess() }).unwrap_or(0.0);
        Timings {
            origin,
            origin_ms,
            phases: Vec::new(),
        }
    }

    /// Records that `name` was reached now.
    pub fn mark(&mut self, name: &'static str) {
        self.record(name, Instant::now());
    }

    /// Records that `name` was reached at `at`, which may be on another
    /// thread.
    pub fn record(&mut self, name: &'static str, at: Instant) {
        self.phases.push((name, at));
    }

    /// Records that `name` was reached at `at`, unless it was reached before.
    /// Start-up timings are about the first image, not those opened later.
    pub fn record_first(&mut self, name: &'static str, at: Instant) {
        if self.phases.iter().all(|&(recorded, _)| recorded != name) {
            self.record(name, at);
        }
    }

    /// Records that `name` was reached at `at`, in place of any earlier time.
    /// Phases about the latest of something, such as the latest move to
    /// another image, use this.
    pub fn record_latest(&mut self, name: &'static str, at: Instant) {
        self.forget(name);
        self.record(name, at);
    }

    /// Forgets that `name` was reached.
    pub fn forget(&mut self, name: &'static str) {
        self.phases.retain(|&(recorded, _)| recorded != name);
    }

    /// The milliseconds from the process's creation to `at`.
    pub fn ms(&self, at: Instant) -> f64 {
        let since = if at >= self.origin {
            at.duration_since(self.origin).as_secs_f64()
        } else {
            -self.origin.duration_since(at).as_secs_f64()
        };
        self.origin_ms + since * 1000.0
    }

    /// The phases, earliest first.
    pub fn report(&self) -> Report {
        let mut phases: Vec<Phase> = self
            .phases
            .iter()
            .map(|&(name, at)| Phase {
                name: name.to_owned(),
                ms: self.ms(at),
            })
            .collect();
        phases.sort_by(|a, b| a.ms.total_cmp(&b.ms));
        Report { phases }
    }
}

/// The milliseconds from the creation of `process` to now, on the clock of
/// the phases in [`Timings::report`] when `process` is a viewer, or `None` if
/// Windows cannot say when the process was created. `process` needs query
/// access. Another process uses this to place a moment of its own on a
/// viewer's timeline, as the open benchmark does.
pub fn process_age_ms(process: HANDLE) -> Option<f64> {
    // SAFETY: GetSystemTimePreciseAsFileTime has no arguments.
    let now = unsafe { GetSystemTimePreciseAsFileTime() };
    let mut created = FILETIME::default();
    let mut unused = [FILETIME::default(); 3];
    // SAFETY: an invalid handle makes the call fail rather than misbehave,
    // and every pointer is to a live local.
    unsafe {
        GetProcessTimes(
            process,
            &mut created,
            &mut unused[0],
            &mut unused[1],
            &mut unused[2],
        )
    }
    .ok()?;
    Some(elapsed_ms(ticks(created), ticks(now)))
}

/// A `FILETIME` as one count of 100-nanosecond ticks.
fn ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// The milliseconds from one tick count to a later one.
fn elapsed_ms(from: u64, to: u64) -> f64 {
    to.saturating_sub(from) as f64 / 10_000.0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn phases_count_from_process_creation_in_order() {
        let mut timings = Timings::start();
        // The test process started before this test did.
        assert!(timings.origin_ms > 0.0);
        let earlier = timings.origin;
        timings.mark("second");
        timings.record("first", earlier);
        timings.record("before", earlier - Duration::from_millis(1));
        timings.record_first("first", earlier + Duration::from_secs(1));
        let report = timings.report();
        let names: Vec<&str> = report.phases.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["before", "first", "second"]);
        assert!((report.phases[1].ms - timings.origin_ms).abs() < 1e-9);
        assert!((report.phases[1].ms - report.phases[0].ms - 1.0).abs() < 1e-6);

        timings.record_latest("first", earlier + Duration::from_secs(2));
        timings.forget("before");
        let names: Vec<String> = timings
            .report()
            .phases
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["second", "first"]);
    }

    #[test]
    fn process_age_counts_from_creation() {
        // SAFETY: the pseudo-handle for the current process is always valid.
        let me = unsafe { GetCurrentProcess() };
        let first = process_age_ms(me).expect("the current process has an age");
        let second = process_age_ms(me).expect("the current process has an age");
        assert!(first > 0.0);
        assert!(second >= first);
        assert_eq!(process_age_ms(HANDLE::default()), None);
    }

    #[test]
    fn file_times() {
        let time = FILETIME {
            dwLowDateTime: 5,
            dwHighDateTime: 1,
        };
        assert_eq!(ticks(time), (1 << 32) + 5);
        assert_eq!(elapsed_ms(10_000, 35_000), 2.5);
        assert_eq!(elapsed_ms(35_000, 10_000), 0.0);
    }
}
