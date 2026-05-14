//! Embeds libmetal_hook.dylib into the smeltr binary at build time and
//! extracts it to a stable on-disk path at runtime so dyld can load it
//! via DYLD_INSERT_LIBRARIES.
//!
//! Why on disk: dyld requires a real filesystem path (it mmaps the file
//! into the child process). The extracted path is deterministic per
//! release so concurrent `smeltr record` calls share one copy and the
//! file is not rewritten on every invocation.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const EMBEDDED_DYLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libmetal_hook.dylib"));

fn dylib_fingerprint() -> &'static str {
    use sha2::{Digest, Sha256};
    use std::sync::OnceLock;
    static FP: OnceLock<String> = OnceLock::new();
    FP.get_or_init(|| {
        let mut h = Sha256::new();
        h.update(EMBEDDED_DYLIB);
        let digest = h.finalize();
        hex::encode(&digest[..6])
    })
}

/// Default extraction directory: `$TMPDIR` (already cleaned up by macOS).
pub fn default_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Extract the embedded dylib to `dir/libmetal_hook-<fingerprint>.dylib`.
/// Returns the absolute path. Skips the write if the file already has
/// the right content; rewrites if content differs.
pub fn extract_to(dir: &Path) -> io::Result<PathBuf> {
    let name = format!("libmetal_hook-{}.dylib", dylib_fingerprint());
    let path = dir.join(name);
    if path.exists() {
        if let Ok(existing) = std::fs::read(&path) {
            if existing == EMBEDDED_DYLIB {
                return Ok(path);
            }
        }
    }
    std::fs::write(&path, EMBEDDED_DYLIB)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Convenience: extract to the default tmp dir.
pub fn extract() -> io::Result<PathBuf> {
    extract_to(&default_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn extract_writes_dylib_with_executable_perms() {
        let dir = tempfile::tempdir().unwrap();
        let path = extract_to(dir.path()).unwrap();
        assert!(path.exists());
        let perm = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perm.mode() & 0o777, 0o755);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes, EMBEDDED_DYLIB);
    }

    #[test]
    fn extract_is_idempotent_when_content_matches() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = extract_to(dir.path()).unwrap();
        let mtime1 = std::fs::metadata(&p1).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let p2 = extract_to(dir.path()).unwrap();
        let mtime2 = std::fs::metadata(&p2).unwrap().modified().unwrap();
        assert_eq!(p1, p2);
        assert_eq!(mtime1, mtime2, "file was rewritten unnecessarily");
    }

    #[test]
    fn extract_rewrites_when_content_differs() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = extract_to(dir.path()).unwrap();
        std::fs::write(&p1, b"stale").unwrap();
        let p2 = extract_to(dir.path()).unwrap();
        assert_eq!(p1, p2);
        let bytes = std::fs::read(&p2).unwrap();
        assert_eq!(bytes, EMBEDDED_DYLIB, "stale content was not refreshed");
    }
}
