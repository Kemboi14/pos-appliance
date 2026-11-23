//! Entropy from RDSEED, else RDRAND, with the same health checks as Tier A.
//! No source means `fill` fails and the app refuses operations that need
//! fresh randomness.

use core::arch::x86_64::{__cpuid, __cpuid_count, _rdrand64_step, _rdseed64_step};
use pos_hal::{HalError, HalResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    RdSeed,
    RdRand,
    None,
}

#[derive(Debug)]
pub struct Entropy {
    pub source: Source,
}

#[target_feature(enable = "rdseed")]
fn rdseed() -> Option<u64> {
    let mut v = 0u64;
    for _ in 0..100 {
        if _rdseed64_step(&mut v) == 1 {
            return Some(v);
        }
        core::hint::spin_loop();
    }
    None
}

#[target_feature(enable = "rdrand")]
fn rdrand() -> Option<u64> {
    let mut v = 0u64;
    for _ in 0..10 {
        if _rdrand64_step(&mut v) == 1 {
            return Some(v);
        }
    }
    None
}

impl Entropy {
    pub fn init() -> Entropy {
        let rdrand = __cpuid(1).ecx & (1 << 30) != 0;
        let rdseed = __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 18) != 0;
        let source = if rdseed {
            Source::RdSeed
        } else if rdrand {
            Source::RdRand
        } else {
            Source::None
        };
        Entropy { source }
    }

    fn word(&self) -> Option<u64> {
        match self.source {
            // SAFETY: CPU support was checked in `init`.
            Source::RdSeed => unsafe { rdseed() },
            // SAFETY: CPU support was checked in `init`.
            Source::RdRand => unsafe { rdrand() },
            Source::None => None,
        }
    }
}

impl pos_hal::Rng for Entropy {
    fn fill(&mut self, buf: &mut [u8]) -> HalResult<()> {
        let mut last = None;
        for chunk in buf.chunks_mut(8) {
            let w = self.word().ok_or(HalError::EntropyFailure)?;
            // Health check: stuck output or all-0/all-1 words.
            if w == 0 || w == u64::MAX || Some(w) == last {
                return Err(HalError::EntropyFailure);
            }
            last = Some(w);
            chunk.copy_from_slice(&w.to_le_bytes()[..chunk.len()]);
        }
        Ok(())
    }
}
