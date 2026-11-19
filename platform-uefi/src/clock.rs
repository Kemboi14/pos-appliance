//! Wall clock from RuntimeServices.GetTime; monotonic time from the TSC,
//! calibrated against a 50 ms boot-services Stall at startup.

use core::time::Duration;
use pos_hal::{DateTime, HalError, HalResult};
use uefi::{boot, runtime};

#[derive(Debug)]
pub struct TscClock {
    t0: u64,
    ticks_per_us: u64,
}

fn rdtsc() -> u64 {
    // SAFETY: RDTSC is available on every x86_64 CPU and has no side effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

impl TscClock {
    pub fn calibrate() -> TscClock {
        let a = rdtsc();
        boot::stall(Duration::from_millis(50));
        let b = rdtsc();
        // Guard against a broken TSC (some hypervisors): fall back to 1 GHz.
        let tpu = (b.wrapping_sub(a) / 50_000).max(1);
        let tpu = if tpu < 100 { 1000 } else { tpu };
        TscClock {
            t0: b,
            ticks_per_us: tpu,
        }
    }
}

impl pos_hal::Clock for TscClock {
    fn monotonic_us(&self) -> u64 {
        rdtsc().wrapping_sub(self.t0) / self.ticks_per_us
    }
    fn wall(&self) -> HalResult<DateTime> {
        let t = runtime::get_time().map_err(|_| HalError::Io)?;
        Ok(DateTime {
            year: t.year(),
            month: t.month(),
            day: t.day(),
            hour: t.hour(),
            minute: t.minute(),
            second: t.second(),
        })
    }
}
