//! Directory listing for the file picker.
//!
//! The Vita has no native photo-picker dialog, so the picker is a custom
//! filesystem browser. On hardware we list directories via the Sce IO
//! dirent API (`sceIoDopen`/`sceIoDread`/`sceIoDclose`); on host we back
//! it with `std::fs::read_dir` so the picker logic is unit-testable
//! without the SDK.
//!
//! Sce paths are `drive:/path` form (e.g. `ux0:picture/`). `sceIoDread`
//! does not return `.`/`..`, but we filter them defensively anyway.

use std::io;

/// One entry in a directory listing. Names are lossy-decoded UTF-8 (Sce
/// `d_name` is a 256-byte buffer with no encoding guarantee).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// List the entries of `path`. Directories are flagged via `is_dir` so
/// the picker can render folder vs file affordances. Returns entries in
/// the order the filesystem yields them (caller sorts).
#[cfg(target_os = "vita")]
pub fn read_dir(path: &str) -> io::Result<Vec<DirEntry>> {
    use std::ffi::CString;
    use vitasdk_sys as sce;

    let c_path = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;

    // SAFETY: c_path is a valid NUL-terminated C string for the duration
    // of the call.
    let fd = unsafe { sce::sceIoDopen(c_path.as_ptr()) };
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("sceIoDopen({path}) failed: {fd:#x}"),
        ));
    }

    let mut out = Vec::new();
    loop {
        // SAFETY: dirent is a fully-owned zeroed POD struct; sceIoDread
        // populates d_stat + d_name. Return: >0 more entries, 0 done,
        // <0 error.
        let mut dirent: sce::SceIoDirent = unsafe { core::mem::zeroed() };
        let res = unsafe { sce::sceIoDread(fd, &mut dirent) };
        if res <= 0 {
            break;
        }

        // Decode d_name up to the first NUL. `c as u8` is correct whether
        // the target's c_char is signed or unsigned.
        let bytes: Vec<u8> = dirent
            .d_name
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        let name = String::from_utf8_lossy(&bytes).into_owned();

        // Vita quirk: the directory bit can live in `st_mode` (SCE_S_*)
        // or in `st_attr` (SCE_SO_*) depending on the mount/filesystem —
        // the gallery subfolders under ux0:picture/ flag it via st_attr.
        // Check both so folders are never mistaken for files.
        let mode_dir = (dirent.d_stat.st_mode as u32) & sce::SCE_S_IFMT == sce::SCE_S_IFDIR;
        let attr_dir = dirent.d_stat.st_attr & sce::SCE_SO_IFMT == sce::SCE_SO_IFDIR;
        let is_dir = mode_dir || attr_dir;

        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        out.push(DirEntry { name, is_dir });
    }

    // SAFETY: fd is a valid open dir handle from sceIoDopen.
    unsafe {
        sce::sceIoDclose(fd);
    }
    Ok(out)
}

/// Host fallback: real listing via `std::fs` so the picker is testable
/// off-device.
#[cfg(not(target_os = "vita"))]
pub fn read_dir(path: &str) -> io::Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if name == "." || name == ".." {
            continue;
        }
        out.push(DirEntry { name, is_dir });
    }
    Ok(out)
}

#[cfg(all(test, not(target_os = "vita")))]
mod tests {
    use super::*;

    #[test]
    fn lists_crate_dir() {
        // `cargo test` runs with cwd = crate root, so Cargo.toml + src/
        // are present.
        let entries = read_dir(".").expect("read_dir(.) should succeed");
        assert!(!entries.is_empty(), "crate dir should not be empty");
        assert!(
            entries.iter().any(|e| e.name == "Cargo.toml" && !e.is_dir),
            "Cargo.toml should be listed as a file: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.name == "src" && e.is_dir),
            "src should be listed as a dir: {entries:?}"
        );
    }

    #[test]
    fn missing_dir_errors() {
        assert!(read_dir("./definitely-not-a-real-dir-xyz").is_err());
    }
}
