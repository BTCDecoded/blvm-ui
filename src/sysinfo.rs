//! Facts about the computer the node will run on that decide whether the official release can start at all.
//!
//! Today that is one fact: the memory page size. The official linux-aarch64 release is built with jemalloc, whose
//! page size is fixed when it is compiled; a kernel with bigger pages makes the program abort at its first
//! allocation. The console reads the page size before it downloads anything and says so in plain words.

use serde::{Deserialize, Serialize};

/// The page size the official linux-aarch64 release (v0.1.60) is built for: jemalloc at 4 KB pages. It aborts with
/// `<jemalloc>: Unsupported system page size` on 16 KB kernels (Raspberry Pi 5).
pub const RELEASE_PAGE_SIZE: u64 = 4096;

/// What the aborting program prints (compared without regard to case).
pub const PAGE_SIZE_ERROR_TEXT: &str = "unsupported system page size";

/// `BLVM_UI_PAGE_SIZE=16384` pretends this computer uses that page size. For trying the warning on a machine that
/// has 4 KB pages; never set by a real install.
pub const PAGE_SIZE_ENV: &str = "BLVM_UI_PAGE_SIZE";

/// The memory page size of this computer in bytes (`sysconf(_SC_PAGESIZE)`); `None` when it cannot be read
/// (not a Unix system, or the call failed). [`PAGE_SIZE_ENV`] overrides it.
pub fn page_size() -> Option<u64> {
    if let Some(n) = std::env::var(PAGE_SIZE_ENV).ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|n| n.is_power_of_two()) {
        return Some(n);
    }
    real_page_size()
}

#[cfg(unix)]
fn real_page_size() -> Option<u64> {
    // SAFETY: `sysconf` has no preconditions; it returns -1 for a name it does not know.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(n).ok().filter(|n| *n > 0)
}

#[cfg(not(unix))]
fn real_page_size() -> Option<u64> {
    None
}

/// `16 KB` for 16384; `larger` when the size is not known.
fn kb_words(page: Option<u64>) -> String {
    match page {
        Some(p) => format!("{} KB", p / 1024),
        None => "larger".into(),
    }
}

/// The plain explanation shown instead of the raw jemalloc lines, and before a download.
pub fn page_size_message(page: Option<u64>) -> String {
    format!(
        "This device uses {} memory pages. The current official node release is built for 4 KB pages and does not start here \
         (jemalloc: unsupported system page size). Options: wait for a release built with 16 KB page support, which has been \
         requested from Bitcoin Commons; or, on a Raspberry Pi 5, switch the kernel to 4 KB pages (add kernel=kernel8.img \
         to /boot/firmware/config.txt and reboot; this is a system change, do it only if you know what it means).",
        kb_words(page)
    )
}

/// Whether the official release for `platform` (`linux-x86_64`, `linux-aarch64`, `unsupported`) can start on a
/// computer with this page size. Only Linux aarch64 can have pages other than 4 KB in practice; on x86_64 Linux they
/// are always 4 KB, and with no asset for the platform (macOS and others) the question does not arise.
pub fn runs_here(platform: &str, page: Option<u64>) -> Result<(), String> {
    match (platform, page) {
        ("linux-aarch64", Some(p)) if p > RELEASE_PAGE_SIZE => Err(page_size_message(Some(p))),
        _ => Ok(()),
    }
}

/// [`runs_here`] for this computer's own platform and page size.
pub fn release_runs_here() -> Result<(), String> {
    runs_here(&crate::releases::platform(), page_size())
}

/// What `state.json` (`platform`) and the status carry about one computer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Facts {
    /// Memory page size in bytes; `None` when it could not be read.
    pub page_size: Option<u64>,
    /// The official release can start here.
    pub runs_here: bool,
    /// Plain words for `runs_here == false`.
    pub reason: Option<String>,
}

impl Default for Facts {
    fn default() -> Self {
        Self { page_size: None, runs_here: true, reason: None }
    }
}

impl Facts {
    /// The facts for a platform and page size.
    pub fn for_page(platform: &str, page: Option<u64>) -> Self {
        match runs_here(platform, page) {
            Ok(()) => Self { page_size: page, runs_here: true, reason: None },
            Err(why) => Self { page_size: page, runs_here: false, reason: Some(why) },
        }
    }

    /// This process's own platform and page size.
    pub fn detect() -> Self {
        Self::for_page(&crate::releases::platform(), page_size())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_size_of_this_computer_is_read_and_is_a_power_of_two() {
        // Not through `page_size()`: the override variable must not matter to what is being checked.
        let p = real_page_size();
        #[cfg(unix)]
        {
            let p = p.expect("sysconf answers on a Unix system");
            assert!(p.is_power_of_two() && p >= 4096, "{p}");
        }
        #[cfg(not(unix))]
        assert_eq!(p, None);
    }

    #[test]
    fn only_linux_aarch64_with_big_pages_cannot_run_the_release() {
        assert!(runs_here("linux-aarch64", Some(4096)).is_ok());
        assert!(runs_here("linux-aarch64", None).is_ok(), "unknown: let the test run decide");
        let why = runs_here("linux-aarch64", Some(16384)).unwrap_err();
        assert!(why.starts_with("This device uses 16 KB memory pages."), "{why}");
        assert!(why.contains("kernel=kernel8.img") && why.contains("/boot/firmware/config.txt"), "{why}");
        assert!(why.contains("requested from Bitcoin Commons") && why.contains("unsupported system page size"), "{why}");
        assert!(runs_here("linux-aarch64", Some(65536)).unwrap_err().contains("64 KB"));
        // x86_64 Linux: pages are 4 KB; if one reported more, the aarch64 jemalloc build is not what it would run.
        assert!(runs_here("linux-x86_64", Some(16384)).is_ok());
        // macOS (and anything else without an official asset): not applicable, whatever the page size (16 KB on
        // Apple silicon).
        assert!(runs_here("unsupported", Some(16384)).is_ok());
    }

    #[test]
    fn facts_carry_the_page_size_and_the_reason() {
        let f = Facts::for_page("linux-aarch64", Some(16384));
        assert_eq!((f.page_size, f.runs_here), (Some(16384), false));
        assert!(f.reason.as_deref().unwrap().contains("16 KB"));
        let f = Facts::for_page("unsupported", Some(16384));
        assert_eq!((f.page_size, f.runs_here, f.reason), (Some(16384), true, None));
        // A file from an agent that wrote nothing about it reads as "runs".
        let f: Facts = serde_json::from_str("{}").unwrap();
        assert!(f.runs_here && f.page_size.is_none());
    }
}
