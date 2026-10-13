use std::{env, fs, path::PathBuf};

// Put memory.x on the linker search path so cortex-m-rt's link.x can include it.
fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
}
