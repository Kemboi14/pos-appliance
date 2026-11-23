//! 16550 UART on the legacy COM ports: COM1 = kernel log (debug builds
//! only), COM2 = ESC/POS serial printer fallback (as in Tier A's QEMU setup).

use crate::cpu::{inb, outb};
use alloc::string::String;
use pos_hal::{HalError, HalResult, PrinterStatus};

pub const COM1: u16 = 0x3f8;
pub const COM2: u16 = 0x2f8;

#[derive(Debug, Clone, Copy)]
pub struct Uart {
    base: u16,
}

impl Uart {
    /// Initialise 115200 8N1 with FIFOs. Returns `None` when the loopback
    /// test fails (no UART at this address).
    pub fn init(base: u16) -> Option<Uart> {
        outb(base + 1, 0x00); // no interrupts
        outb(base + 3, 0x80); // DLAB
        outb(base, 0x01); // divisor 1 = 115200
        outb(base + 1, 0x00);
        outb(base + 3, 0x03); // 8N1
        outb(base + 2, 0xc7); // FIFO on, cleared, 14-byte threshold
        outb(base + 4, 0x1e); // loopback
        outb(base, 0xae);
        if inb(base) != 0xae {
            return None;
        }
        outb(base + 4, 0x0f); // normal operation, DTR/RTS/OUT1/OUT2
        Some(Uart { base })
    }

    fn put(&self, b: u8) -> HalResult<()> {
        // ~1 s at 115200 baud is far beyond one byte time.
        for _ in 0..1_000_000 {
            if inb(self.base + 5) & 0x20 != 0 {
                outb(self.base, b);
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(HalError::Timeout)
    }

    pub fn write_bytes(&self, bytes: &[u8]) -> HalResult<()> {
        for &b in bytes {
            self.put(b)?;
        }
        // Wait for the transmitter to drain (THR and shift register empty).
        for _ in 0..1_000_000 {
            if inb(self.base + 5) & 0x40 != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(HalError::Timeout)
    }
}

/// Log sink used by `klog!` (debug-console builds only). Port I/O only, so no locking is needed on a
/// single CPU with interrupts off.
#[cfg(feature = "debug-console")]
#[derive(Debug)]
pub struct Log;

#[cfg(feature = "debug-console")]
impl core::fmt::Write for Log {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let u = Uart { base: COM1 };
        for b in s.bytes() {
            if b == b'\n' {
                let _ = u.put(b'\r');
            }
            let _ = u.put(b);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! klog {
    ($($arg:tt)*) => {{
        #[cfg(feature = "debug-console")]
        {
            use core::fmt::Write as _;
            let _ = writeln!($crate::serial::Log, $($arg)*);
        }
        #[cfg(not(feature = "debug-console"))]
        {
            let _ = format_args!($($arg)*);
        }
    }};
}

/// ESC/POS printer on COM2.
#[derive(Debug)]
pub struct SerialPrinter {
    port: Option<Uart>,
}

impl SerialPrinter {
    pub fn init() -> SerialPrinter {
        SerialPrinter {
            port: Uart::init(COM2),
        }
    }
}

impl pos_hal::Printer for SerialPrinter {
    fn write(&mut self, bytes: &[u8]) -> HalResult<()> {
        self.port.ok_or(HalError::NotPresent)?.write_bytes(bytes)
    }
    fn status(&mut self) -> PrinterStatus {
        if self.port.is_some() {
            PrinterStatus::Ready
        } else {
            PrinterStatus::NotPresent
        }
    }
    fn describe(&self) -> String {
        match self.port {
            Some(_) => String::from("serial printer (COM2)"),
            None => String::from("no printer"),
        }
    }
}
