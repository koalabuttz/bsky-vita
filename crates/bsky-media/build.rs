//! Static-link turbojpeg (software JPEG encoder) when targeting Vita.
//!
//! `jpeg::encode_rgba` calls into `libturbojpeg.a` (ships with the
//! vitasdk). The linker (`arm-vita-eabi-gcc`) searches
//! `$VITASDK/arm-vita-eabi/lib/` by default. We re-list `jpeg` after
//! `turbojpeg` so turbojpeg's references into the IJG codec resolve even
//! if another crate's `-ljpeg` was already scanned. Host builds compile
//! to stubs and need nothing.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("vita") {
        for lib in ["turbojpeg", "jpeg"] {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
