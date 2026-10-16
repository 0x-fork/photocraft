//! A recovered PSD may be exported only to a new file. Publishing a completely
//! written temporary inode with `hard_link` gives an atomic, no-replace commit:
//! replacing the original file (including through an alias) is impossible.
//!
//! There is deliberately no fallback to `rename`, which can replace files.

use crate::IoError;
use std::path::Path;

#[cfg(not(target_arch = "wasm32"))]
struct Temporary(std::path::PathBuf);

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Atomically publish an encoded recovery copy at a **previously unused** name.
/// Unlike a conventional Save As, this NEVER replaces an existing destination.
/// This includes aliases of the original (symlinks/hardlinks) and defeats a
/// race where a destination appears after an earlier `exists()` check.
/// A filesystem that cannot hard-link a same-directory temporary file fails
/// closed rather than falling back to a destructive operation.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_recovery_copy_bytes(destination: &Path, bytes: &[u8]) -> Result<(), IoError> {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let err = |e: std::io::Error| IoError::Unsupported(format!("recovery copy: {e}"));
    let parent = destination.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = destination.file_name().filter(|n| !n.is_empty()).ok_or_else(|| IoError::Unsupported("recovery copy needs a valid file name".into()))?;
    let mut temporary = None;
    for _ in 0..64 {
        let next = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut filename = std::ffi::OsString::from(".photocraft-recovery-");
        filename.push(std::process::id().to_string());
        filename.push(format!("-{next}-"));
        filename.push(name);
        filename.push(".tmp");
        let candidate = parent.join(filename);
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(file) => {
                temporary = Some((Temporary(candidate), file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(err(e)),
        }
    }
    let (temp, mut file) = temporary.ok_or_else(|| IoError::Unsupported("cannot reserve a temporary recovery file".into()))?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)?;
    drop(file);
    // The link operation is atomic: EEXIST if ANY file was placed at the
    // destination, even after a previous validation or through a symlink.
    std::fs::hard_link(&temp.0, destination).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            IoError::Unsupported("recovery copy must use a NEW file name; an existing file will never be overwritten".into())
        } else {
            err(e)
        }
    })?;
    // The copy now has its own name and stays intact when the temp name is removed.
    Ok(())
}

#[cfg(target_arch = "wasm32")]
pub fn write_recovery_copy_bytes(_destination: &Path, _bytes: &[u8]) -> Result<(), IoError> {
    Err(IoError::Unsupported("native recovery copy is unavailable on the web".into()))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn recovered_copy_never_replaces_an_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "photocraft-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("original.psd");
        let copy = dir.join("copy.psd");
        std::fs::write(&source, b"valuable original").unwrap();
        assert!(write_recovery_copy_bytes(&source, b"damaged").is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"valuable original");
        write_recovery_copy_bytes(&copy, b"recovered").unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"recovered");
        assert!(write_recovery_copy_bytes(&copy, b"overwritten").is_err());
        assert_eq!(std::fs::read(&copy).unwrap(), b"recovered");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = dir.join("symlink.psd");
            symlink(&source, &link).unwrap();
            assert!(write_recovery_copy_bytes(&link, b"symlink overwrite").is_err());
            let hardlink = dir.join("hardlink.psd");
            std::fs::hard_link(&source, &hardlink).unwrap();
            assert!(write_recovery_copy_bytes(&hardlink, b"hardlink overwrite").is_err());
        }
        assert_eq!(std::fs::read(&source).unwrap(), b"valuable original");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
