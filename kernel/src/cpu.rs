//! CPU setup: port I/O, our own GDT/TSS (IST stacks for #DF/#NMI/#MC) and an
//! IDT whose exception handlers log and reset. Interrupts stay disabled in
//! step 1; every device is polled.

use core::arch::{asm, naked_asm};
use core::cell::UnsafeCell;

// ---------------------------------------------------------------- port I/O

pub fn outb(port: u16, v: u8) {
    // SAFETY: port I/O has no memory effects; callers only touch legacy PC
    // ports (COM, CMOS, PIT, KBC) that are safe to access from ring 0.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags))
    }
}

pub fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: as for `outb`.
    unsafe {
        asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags))
    }
    v
}

pub fn rdtsc() -> u64 {
    // SAFETY: RDTSC exists on every x86_64 CPU and has no side effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Cell for tables that are written once during single-threaded init and
/// then only read by the CPU.
#[repr(transparent)]
struct InitCell<T>(UnsafeCell<T>);
// SAFETY: single CPU, written only in `init` before interrupts could exist.
unsafe impl<T> Sync for InitCell<T> {}

// ---------------------------------------------------------------- GDT / TSS

pub const KCODE: u16 = 0x08;
pub const KDATA: u16 = 0x10;
const TSS_SEL: u16 = 0x18;

#[repr(C, packed(4))]
struct Tss {
    _r0: u32,
    rsp: [u64; 3],
    _r1: u64,
    ist: [u64; 7],
    _r2: u64,
    _r3: u16,
    iomap: u16,
}

const IST_SIZE: usize = 16 * 1024;
#[repr(C, align(16))]
struct IstStack([u8; IST_SIZE]);

static IST_STACKS: InitCell<[IstStack; 3]> =
    InitCell(UnsafeCell::new([const { IstStack([0; IST_SIZE]) }; 3]));
static TSS: InitCell<Tss> = InitCell(UnsafeCell::new(Tss {
    _r0: 0,
    rsp: [0; 3],
    _r1: 0,
    ist: [0; 7],
    _r2: 0,
    _r3: 0,
    iomap: core::mem::size_of::<Tss>() as u16,
}));
static GDT: InitCell<[u64; 5]> = InitCell(UnsafeCell::new([
    0,
    0x00af_9a00_0000_ffff, // 0x08 kernel code: L=1, P, DPL0, exec/read
    0x00cf_9200_0000_ffff, // 0x10 kernel data
    0,                     // 0x18 TSS (low), filled in at init
    0,                     // 0x20 TSS (high)
]));

#[repr(C, packed)]
struct Pointer {
    limit: u16,
    base: u64,
}

// IST indices (1-based in the IDT entry).
const IST_DF: u8 = 1;
const IST_NMI: u8 = 2;
const IST_MC: u8 = 3;

fn init_gdt() {
    // SAFETY: single-threaded init; nothing else references these statics yet.
    unsafe {
        let stacks = &mut *IST_STACKS.0.get();
        let tss = &mut *TSS.0.get();
        for (i, s) in stacks.iter_mut().enumerate() {
            tss.ist[i] = s.0.as_mut_ptr().add(IST_SIZE) as u64;
        }
        let base = TSS.0.get() as u64;
        let limit = (core::mem::size_of::<Tss>() - 1) as u64;
        let gdt = &mut *GDT.0.get();
        gdt[3] = (limit & 0xffff)
            | ((base & 0xff_ffff) << 16)
            | (0x89 << 40) // present, 64-bit TSS (available)
            | (((limit >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        gdt[4] = base >> 32;

        let ptr = Pointer {
            limit: (core::mem::size_of::<[u64; 5]>() - 1) as u16,
            base: GDT.0.get() as u64,
        };
        asm!(
            "lgdt [{p}]",
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov ss, {ds:x}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            "ltr {tss:x}",
            p = in(reg) &ptr,
            cs = const KCODE as u64,
            ds = in(reg) KDATA as u64,
            tss = in(reg) TSS_SEL as u64,
            tmp = out(reg) _,
        );
    }
}

// ---------------------------------------------------------------- IDT

#[derive(Clone, Copy)]
#[repr(C)]
struct IdtEntry {
    off_lo: u16,
    sel: u16,
    ist: u8,
    attr: u8,
    off_mid: u16,
    off_hi: u32,
    _r: u32,
}

impl IdtEntry {
    const MISSING: IdtEntry = IdtEntry {
        off_lo: 0,
        sel: 0,
        ist: 0,
        attr: 0,
        off_mid: 0,
        off_hi: 0,
        _r: 0,
    };
    fn new(handler: u64, ist: u8) -> Self {
        IdtEntry {
            off_lo: handler as u16,
            sel: KCODE,
            ist,
            attr: 0x8e, // present, DPL0, 64-bit interrupt gate
            off_mid: (handler >> 16) as u16,
            off_hi: (handler >> 32) as u32,
            _r: 0,
        }
    }
}

static IDT: InitCell<[IdtEntry; 256]> = InitCell(UnsafeCell::new([IdtEntry::MISSING; 256]));

/// Stack frame as laid out by the stubs: vector, error code (0 if the CPU
/// pushed none), then the CPU's interrupt frame.
#[repr(C)]
#[derive(Debug)]
pub struct ExceptionFrame {
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

macro_rules! stubs {
    ($( $name:ident = $v:literal $kind:ident ),* $(,)?) => {
        $( stubs!(@one $name $v $kind); )*
        const STUBS: [extern "C" fn(); 32] = [$($name),*];
    };
    (@one $name:ident $v:literal noerr) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!("push 0", "push {v}", "jmp {c}", v = const $v, c = sym exception_common)
        }
    };
    (@one $name:ident $v:literal err) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!("push {v}", "jmp {c}", v = const $v, c = sym exception_common)
        }
    };
}

stubs! {
    e0 = 0 noerr, e1 = 1 noerr, e2 = 2 noerr, e3 = 3 noerr,
    e4 = 4 noerr, e5 = 5 noerr, e6 = 6 noerr, e7 = 7 noerr,
    e8 = 8 err, e9 = 9 noerr, e10 = 10 err, e11 = 11 err,
    e12 = 12 err, e13 = 13 err, e14 = 14 err, e15 = 15 noerr,
    e16 = 16 noerr, e17 = 17 err, e18 = 18 noerr, e19 = 19 noerr,
    e20 = 20 noerr, e21 = 21 err, e22 = 22 noerr, e23 = 23 noerr,
    e24 = 24 noerr, e25 = 25 noerr, e26 = 26 noerr, e27 = 27 noerr,
    e28 = 28 noerr, e29 = 29 err, e30 = 30 err, e31 = 31 noerr,
}

#[unsafe(naked)]
extern "C" fn exception_common() {
    naked_asm!(
        "mov rdi, rsp",
        "and rsp, -16",
        "call {h}",
        "ud2",
        h = sym exception_handler,
    )
}

extern "C" fn exception_handler(frame: &ExceptionFrame) -> ! {
    crate::klog!(
        "EXCEPTION {} err={:#x} rip={:#x} rsp={:#x} cr2={:#x}",
        frame.vector,
        frame.error,
        frame.rip,
        frame.rsp,
        read_cr2()
    );
    crate::power::fatal_reset()
}

fn read_cr2() -> u64 {
    let v: u64;
    // SAFETY: reading CR2 has no side effects.
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}

fn init_idt() {
    // SAFETY: single-threaded init; the IDT is only read by the CPU afterwards.
    unsafe {
        let idt = &mut *IDT.0.get();
        for (v, stub) in STUBS.iter().enumerate() {
            let ist = match v {
                2 => IST_NMI,
                8 => IST_DF,
                18 => IST_MC,
                _ => 0,
            };
            idt[v] = IdtEntry::new(*stub as usize as u64, ist);
        }
        let ptr = Pointer {
            limit: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
            base: IDT.0.get() as u64,
        };
        asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));
    }
}

/// Install an interrupt gate for `vector` (>= 32) after `init`.
pub fn set_gate(vector: u8, handler: extern "C" fn()) {
    assert!(vector >= 32);
    // SAFETY: single CPU; called with interrupts disabled, so the CPU never
    // observes a half-written entry.
    unsafe {
        (*IDT.0.get())[usize::from(vector)] = IdtEntry::new(handler as usize as u64, 0);
    }
}

pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: callers only read architectural MSRs the CPU implements.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags))
    };
    u64::from(hi) << 32 | u64::from(lo)
}

pub fn wrmsr(msr: u32, v: u64) {
    // SAFETY: callers only write architectural MSRs with valid values.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, preserves_flags))
    };
}

/// Sleep until the next interrupt. `sti; hlt` is atomic (STI's interrupt
/// shadow), so an interrupt can't slip in between and be missed.
pub fn wait_for_interrupt() {
    // SAFETY: interrupt handlers installed by `apic::init` only touch
    // atomics and the LAPIC; interrupts are disabled again afterwards.
    unsafe { asm!("sti; hlt; cli", options(nomem, nostack)) };
}

/// Load an empty IDT so the next exception triple-faults (last-resort reset).
pub fn triple_fault() -> ! {
    let ptr = Pointer { limit: 0, base: 0 };
    // SAFETY: deliberately resets the machine.
    unsafe { asm!("lidt [{}]", "int3", in(reg) &ptr, options(noreturn)) }
}

pub fn init() {
    init_gdt();
    init_idt();
}
