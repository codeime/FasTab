fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // fterm calls nothing outside libSystem, but `objc2` (through sysinfo and
    // whoami) declares a Foundation link and others add CoreFoundation, IOKit,
    // libobjc and libiconv. Each tab then loaded ~390 images instead of ~80 and
    // ran SkyLight's initializers before `main`: about 0.6 MB of every resident
    // fterm. The linker drops a dylib only when no symbol binds to it, so
    // anything fterm really calls keeps its library.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-dead_strip_dylibs");
    }
}
