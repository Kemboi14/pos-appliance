//! Link the kernel with our own script (higher-half, one PT_LOAD per
//! permission class so Limine maps text RX, rodata R, data RW).
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "x86_64-unknown-none" {
        println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    }
    println!("cargo:rerun-if-changed=linker.ld");
}
