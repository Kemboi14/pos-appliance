//! Monotonic time from the TSC, calibrated against PIT channel 2; wall
//! clock from the CMOS RTC (assumed UTC, as in Tier A; decoding is in
//! `pos_drivers::rtc`).

use crate::cpu::{inb, outb, rdtsc};
use core::sync::atomic::{AtomicU64, Ordering};
use pos_hal::{DateTime, HalError, HalResult};

/// TSC ticks per microsecond. Before calibration we assume a fast CPU, so
/// early `spin_ms` calls err on the long side.
static TICKS_PER_US: AtomicU64 = AtomicU64::new(5_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Calibration {
    Pit,
    CpuidFrequency,
    Guess,
}

const PIT_HZ: u64 = 1_193_182;
const CAL_MS: u64 = 50;

/// Count TSC ticks across a PIT channel-2 one-shot of `CAL_MS`. Returns
/// `None` if the PIT never fires (some newer chipsets gate it off).
fn pit_ticks_per_us() -> Option<u64> {
    let count = PIT_HZ * CAL_MS / 1000;
    // Gate high, speaker off.
    outb(0x61, (inb(0x61) & !0x02) | 0x01);
    outb(0x43, 0xb0); // channel 2, lobyte/hibyte, mode 0 (terminal count)
    outb(0x42, count as u8);
    outb(0x42, (count >> 8) as u8);
    let t0 = rdtsc();
    let mut spins = 0u64;
    while inb(0x61) & 0x20 == 0 {
        spins += 1;
        if spins > 50_000_000 {
            return None;
        }
    }
    let t1 = rdtsc();
    let tpu = t1.wrapping_sub(t0) / (CAL_MS * 1000);
    (tpu >= 100).then_some(tpu)
}

/// CPUID leaf 0x16 base frequency in MHz = ticks/µs on CPUs whose TSC runs
/// at the base clock.
fn cpuid_ticks_per_us() -> Option<u64> {
    use core::arch::x86_64::__cpuid;
    if __cpuid(0).eax < 0x16 {
        return None;
    }
    let mhz = u64::from(__cpuid(0x16).eax & 0xffff);
    (mhz >= 100).then_some(mhz)
}

#[derive(Debug)]
pub struct TscClock {
    tpu: u64,
    pub calibration: Calibration,
    pub invariant: bool,
}

impl TscClock {
    pub fn calibrate() -> TscClock {
        let (tpu, calibration) = match pit_ticks_per_us() {
            Some(t) => (t, Calibration::Pit),
            None => match cpuid_ticks_per_us() {
                Some(t) => (t, Calibration::CpuidFrequency),
                None => (1000, Calibration::Guess),
            },
        };
        TICKS_PER_US.store(tpu, Ordering::Relaxed);
        let ext = core::arch::x86_64::__cpuid(0x8000_0000).eax;
        let invariant =
            ext >= 0x8000_0007 && core::arch::x86_64::__cpuid(0x8000_0007).edx & (1 << 8) != 0;
        TscClock {
            tpu,
            calibration,
            invariant,
        }
    }
    pub fn ticks_per_us(&self) -> u64 {
        self.tpu
    }
}

/// Microseconds since TSC zero (boot), usable before a `TscClock` exists.
pub fn now_us() -> u64 {
    rdtsc() / TICKS_PER_US.load(Ordering::Relaxed)
}

/// Busy-wait `ms` milliseconds (no timer interrupts in step 1).
pub fn spin_ms(ms: u64) {
    let tpu = TICKS_PER_US.load(Ordering::Relaxed);
    let end = rdtsc().wrapping_add(ms * 1000 * tpu);
    while (end.wrapping_sub(rdtsc()) as i64) > 0 {
        core::hint::spin_loop();
    }
}

fn cmos(reg: u8) -> u8 {
    outb(0x70, 0x80 | reg); // keep NMI disabled while selecting
    inb(0x71)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawRtc([u8; 6]);

fn read_raw() -> Option<RawRtc> {
    // Wait out an update in progress (at most ~2 ms once per second).
    for _ in 0..1_000_000 {
        if cmos(0x0a) & 0x80 == 0 {
            return Some(RawRtc([
                cmos(0x00),
                cmos(0x02),
                cmos(0x04),
                cmos(0x07),
                cmos(0x08),
                cmos(0x09),
            ]));
        }
        core::hint::spin_loop();
    }
    None
}

impl pos_hal::Clock for TscClock {
    fn monotonic_us(&self) -> u64 {
        now_us()
    }
    fn wall(&self) -> HalResult<DateTime> {
        wall()
    }
}

/// Wall-clock time from the CMOS RTC.
pub fn wall() -> HalResult<DateTime> {
    // Read until two consecutive reads agree, so we never mix the fields of
    // two different seconds.
    let mut last = read_raw().ok_or(HalError::Timeout)?;
    for _ in 0..5 {
        let now = read_raw().ok_or(HalError::Timeout)?;
        if now == last {
            return pos_drivers::rtc::decode(now.0, cmos(0x0b)).ok_or(HalError::Io);
        }
        last = now;
    }
    Err(HalError::Io)
}
