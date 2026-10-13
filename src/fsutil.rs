//! Small file helpers shared by everything that replaces a file on disk.
//!
//! A file that is renamed into place must first be on the disk itself: `sync_all` on the new file, then on
//! the folder that holds the new name. Without that a power cut right after the rename can leave a name that
//! points at an empty file (ext4 only protects a rename over an existing file).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    /// How many files and folders this thread flushed to the disk through these helpers (tests look at it).
    static SYNCS: Cell<usize> = const { Cell::new(0) };
}
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The number of flushes made so far by the calling thread.
pub fn sync_count() -> usize {
    SYNCS.with(Cell::get)
}

fn count_sync() {
    SYNCS.with(|c| c.set(c.get() + 1));
}

/// `sync_all` on an open file.
pub fn sync_file(f: &std::fs::File) -> std::io::Result<()> {
    count_sync();
    f.sync_all()
}

/// Flush the folder `dir` itself (the new name inside it). Not supported on Windows: skipped there.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        count_sync();
        std::fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// A temp name next to `path` that no other writer (another process, another container with the same small
/// pid under tini, another thread) shares: pid, clock and a counter.
pub fn temp_name(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".tmp{}-{nanos:x}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    PathBuf::from(tmp)
}

/// Replace `path` with `bytes` (its folder must exist): temp file next to it, `sync_all`, rename, then flush the folder. `mode` (Unix
/// permission bits) applies to the new file from the moment it exists, for files that hold a secret.
pub fn write_atomic_bytes(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = temp_name(path);
    let result = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(m);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        sync_file(&f)?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    // The new name is in place; failing to flush the folder is worth a log line, not a failure.
    if let Err(e) = sync_dir(parent) {
        tracing::debug!("cannot flush {}: {e}", parent.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("blvm-fsutil-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn an_atomic_write_flushes_the_file_and_its_folder_and_leaves_no_temp_file() {
        let d = tmp("atomic");
        let p = d.join("sub").join("x.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let before = sync_count();
        write_atomic_bytes(&p, b"{\"a\":1}", None).unwrap();
        assert_eq!(sync_count(), before + 2, "the file and then its folder are synced");
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"a\":1}");
        write_atomic_bytes(&p, b"{\"a\":2}", None).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"a\":2}");
        assert!(std::fs::read_dir(p.parent().unwrap()).unwrap().flatten().all(|e| e.file_name() == "x.json"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_secret_file_is_never_readable_by_others_not_even_for_a_moment() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("mode");
        let p = d.join("secret.toml");
        std::fs::create_dir_all(&d).unwrap();
        write_atomic_bytes(&p, b"password = \"x\"", Some(0o600)).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn temp_names_differ_between_calls() {
        let p = Path::new("/data/networks/signet/meta.json");
        assert_ne!(temp_name(p), temp_name(p));
        assert!(temp_name(p).to_string_lossy().starts_with("/data/networks/signet/meta.json.tmp"));
    }
}
