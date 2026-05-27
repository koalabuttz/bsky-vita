//! Static-link the C codecs we call into when targeting Vita.
//!
//! `jpeg::encode_rgba`/`decode_rgba` call `libturbojpeg.a`, and
//! `image::decode_png` calls libpng's simplified read API (`libpng16.a`,
//! which needs `libz.a`). All ship with the vitasdk; the linker
//! (`arm-vita-eabi-gcc`) searches `$VITASDK/arm-vita-eabi/lib/` by
//! default. Static-link order matters: a library's unresolved references
//! are satisfied only by libraries listed *after* it, so we put `jpeg`
//! after `turbojpeg` and `z` after `png`. (bsky-render already links png/z
//! for FreeType; a duplicate listing is harmless — the linker just
//! re-scans the archive.) Host builds compile to stubs and need nothing.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("vita") {
        for lib in ["turbojpeg", "jpeg", "png", "z"] {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
