//! Emit static-link directives for libvita2d and friends — but ONLY when
//! we're targeting Vita. Host builds (cargo check / cargo test on the dev
//! machine) compile bsky-render down to empty stubs, so they need no
//! native libraries.
//!
//! The linker is `arm-vita-eabi-gcc` (configured in `app/.cargo/config.toml`)
//! and it searches `$VITASDK/arm-vita-eabi/lib/` by default — that's where
//! the .a files live. We just have to name them.
//!
//! `vita2d_ext` resolves the weak PGF/PVF symbols from `vita2d.h`. Without
//! it, calls to `vita2d_load_default_pgf` etc. link but trap at runtime.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("vita") {
        // Static-link order matters for unresolved symbols: a static
        // archive only resolves symbols that have been *referenced* by
        // earlier objects on the command line. vitasdk-sys (a transitive
        // dep) emits its `libSce*_stub` `-l` directives BEFORE ours, so
        // when the linker reaches `libvita2d.a` afterward, the Sce stub
        // archives have already been scanned and their symbols pruned.
        //
        // Re-emitting the relevant stubs AFTER vita2d/vita2d_ext gives the
        // linker a second chance to pick up references vita2d needs
        // (sceGxm*, sceDisplay*, sceCommonDialog*, etc.). Listing a `-l`
        // twice is cheap; the linker just searches the archive again.
        let libs = [
            // Our 2D layer (defines sceGxm* / sceDisplay* references):
            "vita2d",
            "vita2d_ext",
            // Sony module stubs vita2d/vita2d_ext reference. Listed in
            // dependency order: graphics first, then PGF/PVF for the
            // system font loaders, then kernel/sysmem/app-mgr basics.
            "SceGxm_stub",
            "SceDisplay_stub",
            "SceCommonDialog_stub",
            "ScePgf_stub",
            "ScePvf_stub",
            "SceSysmem_stub",
            "SceLibKernel_stub",
            "SceAppMgr_stub",
        ];
        for lib in libs {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }
    // Re-run only if the build script itself changes — we don't depend on
    // any other files for our link decisions.
    println!("cargo:rerun-if-changed=build.rs");
}
