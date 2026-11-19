//! Entropy: EFI_RNG_PROTOCOL if present, else RDSEED, else RDRAND.
//! If none is available or the output fails a basic health check, `fill`
//! fails; the app then refuses operations that need fresh randomness
//! (first-time setup, new users) rather than using weak values.

use core::arch::x86_64::{__cpuid, __cpuid_count, _rdrand64_step, _rdseed64_step};
use pos_hal::{HalError, HalResult};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::rng::Rng as EfiRng;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Efi,
    RdSeed,
    RdRand,
    None,
}

#[derive(Debug)]
pub struct Entropy {
    efi: Option<ScopedProtocol<EfiRng>>,
    pub source: Source,
}

fn has_rdrand() -> bool {
    __cpuid(1).ecx & (1 << 30) != 0
}
fn has_rdseed() -> bool {
    // Leaf 7 is only meaningful when the max basic leaf is >= 7.
    __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 18) != 0
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
        let efi = boot::get_handle_for_protocol::<EfiRng>()
            .ok()
            .and_then(|h| boot::open_protocol_exclusive::<EfiRng>(h).ok());
        let source = if efi.is_some() {
            Source::Efi
        } else if has_rdseed() {
            Source::RdSeed
        } else if has_rdrand() {
            Source::RdRand
        } else {
            Source::None
        };
        Entropy { efi, source }
    }

    fn word(&mut self) -> Option<u64> {
        match self.source {
            // SAFETY: CPU support was checked in `init`.
            Source::RdSeed => unsafe { rdseed() },
            // SAFETY: CPU support was checked in `init`.
            Source::RdRand => unsafe { rdrand() },
            _ => None,
        }
    }
}

impl pos_hal::Rng for Entropy {
    fn fill(&mut self, buf: &mut [u8]) -> HalResult<()> {
        match (&mut self.efi, self.source) {
            (Some(p), _) => p.get_rng(None, buf).map_err(|_| HalError::EntropyFailure)?,
            (None, Source::None) => return Err(HalError::EntropyFailure),
            (None, _) => {
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
            }
        }
        // Reject an all-zero result from any source.
        if buf.len() >= 8 && buf.iter().all(|&b| b == 0) {
            return Err(HalError::EntropyFailure);
        }
        Ok(())
    }
}
