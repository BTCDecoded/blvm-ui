//! Official node releases: what GitHub says is the newest, what this computer can run, and what the
//! installer has put in `<root>/bin/` (managed-node design D, F, M).
//!
//! * [`Cache`] asks `GET repos/BTCDecoded/blvm/releases?per_page=10` (no token, `User-Agent` set), keeps the
//!   answer for ten minutes in memory and in `<state_dir>/releases.json`, and when GitHub cannot be reached
//!   hands out the last answer marked stale (or nothing, with the reason).
//! * [`platform`] names the asset this computer can run: `linux-x86_64`, `linux-aarch64`, or `unsupported`
//!   (there is no macOS or Windows build of the node). `BLVM_UI_PLATFORM` overrides it, for testing the
//!   download path on another system.
//! * [`InstalledRecord`] is `bin/installed.json`, written by the installer: the release tag is what the
//!   user is told is installed (design M.3).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const GITHUB_API: &str = "https://api.github.com/repos/BTCDecoded/blvm/releases";
pub const DOWNLOAD_PREFIX: &str = "https://github.com/BTCDecoded/blvm/releases/download/";
pub const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// "Check now" may bypass the cache this often.
pub const MIN_FORCE_GAP: Duration = Duration::from_secs(60);
const MAX_API_BYTES: usize = 4 * 1024 * 1024;
pub const INSTALLED_FILE: &str = "installed.json";
/// The start of the message for "GitHub does not have that release" (as opposed to "cannot reach GitHub").
const NOT_FOUND_PREFIX: &str = "GitHub has no such release";

/// Is this the error of [`Cache::release`] for a release that does not exist?
pub fn is_not_found(msg: &str) -> bool {
    msg.starts_with(NOT_FOUND_PREFIX)
}

/// Where releases come from. The default is GitHub; tests point it at a local server.
#[derive(Debug, Clone)]
pub struct Source {
    /// `.../releases` (the list); `.../releases/tags/<tag>` is derived from it.
    pub api_url: String,
    /// Every download URL must start with this.
    pub download_prefix: String,
    /// Hosts a redirect may lead to besides `github.com` and `*.githubusercontent.com`.
    pub extra_hosts: Vec<String>,
    /// Refuse anything but https (always true for GitHub).
    pub https_only: bool,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            api_url: GITHUB_API.into(),
            download_prefix: DOWNLOAD_PREFIX.into(),
            extra_hosts: Vec::new(),
            https_only: true,
        }
    }
}

impl Source {
    fn host_ok(&self, url: &reqwest::Url) -> bool {
        if self.https_only && url.scheme() != "https" {
            return false;
        }
        let Some(h) = url.host_str() else { return false };
        let h = h.to_ascii_lowercase();
        h == "github.com"
            || h == "api.github.com"
            || h.ends_with(".githubusercontent.com")
            || self.extra_hosts.iter().any(|x| *x == h)
    }

    /// The address of a download is parsed, not compared as text: scheme, host and port must be the ones of
    /// the download prefix, and the path must stay under the prefix's path once every segment is
    /// percent-decoded (a `%2e%2e` segment is a `..`). Nothing else rides along (login, query, fragment).
    pub fn download_url_ok(&self, url: &str) -> bool {
        let (Ok(u), Ok(prefix)) = (reqwest::Url::parse(url), reqwest::Url::parse(&self.download_prefix)) else {
            return false;
        };
        if u.scheme() != prefix.scheme()
            || u.host_str().map(str::to_ascii_lowercase) != prefix.host_str().map(str::to_ascii_lowercase)
            || u.port_or_known_default() != prefix.port_or_known_default()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return false;
        }
        if self.https_only && u.scheme() != "https" {
            return false;
        }
        // The text as given: no `.` / `..` segment may hide behind percent-encoding, and no encoded slash.
        let raw_path = url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
            .unwrap_or("");
        for seg in raw_path.split('/') {
            match percent_decode(seg) {
                Some(d) if d != "." && d != ".." && !d.contains(['/', '\\', '\0']) => {}
                _ => return false,
            }
        }
        path_under(&u, &prefix)
    }

    /// A client that follows redirects only to hosts this source allows. A hop that stays on the download
    /// host must stay under the download path as well.
    pub fn client(&self) -> Result<reqwest::Client, String> {
        let me = self.clone();
        let prefix = reqwest::Url::parse(&self.download_prefix).ok();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            let on_download_host = prefix.as_ref().is_some_and(|p| {
                attempt.url().host_str().map(str::to_ascii_lowercase) == p.host_str().map(str::to_ascii_lowercase)
            });
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if !me.host_ok(attempt.url()) {
                attempt.error("redirect to a host that is not allowed")
            } else if on_download_host && !prefix.as_ref().is_some_and(|p| path_under(attempt.url(), p)) {
                attempt.error("redirect outside the release downloads")
            } else {
                attempt.follow()
            }
        });
        reqwest::Client::builder()
            .user_agent(format!("blvm-ui-next/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .redirect(policy)
            .build()
            .map_err(|e| format!("Cannot set up the download client: {e}"))
    }
}

/// `%41` -> `A`. `None` for a broken escape or bytes that are not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The (already normalised) path of `u` starts with the path of `prefix`, segment by segment.
fn path_under(u: &reqwest::Url, prefix: &reqwest::Url) -> bool {
    let want: Vec<&str> = prefix.path_segments().map(|s| s.filter(|x| !x.is_empty()).collect()).unwrap_or_default();
    let mut got: Vec<&str> = u.path_segments().map(|s| s.collect()).unwrap_or_default();
    if got.last() == Some(&"") {
        got.pop(); // `.../download/` names the folder, not a file
    }
    got.len() > want.len() && got.iter().all(|x| !x.is_empty()) && got.iter().zip(want.iter()).all(|(a, b)| a == b)
}

// ---- platform ---------------------------------------------------------------------------------

pub const PLATFORM_UNSUPPORTED: &str = "unsupported";

/// The platform of the official asset this computer can run.
pub fn detect_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => PLATFORM_UNSUPPORTED,
    }
}

/// [`detect_platform`], unless `BLVM_UI_PLATFORM` names one of the three values.
pub fn platform() -> String {
    match std::env::var("BLVM_UI_PLATFORM").ok().as_deref().map(str::trim) {
        Some(p @ ("linux-x86_64" | "linux-aarch64" | PLATFORM_UNSUPPORTED)) => p.to_string(),
        _ => detect_platform().to_string(),
    }
}

/// `blvm-0.1.60-linux-x86_64` for tag `v0.1.60`.
pub fn asset_name(tag: &str, platform: &str) -> String {
    format!("blvm-{}-{platform}", tag.trim_start_matches('v'))
}

/// `v0.1.60`, from `0.1.60` or `v0.1.60`. `None` when it does not look like a release number (the tag ends up
/// in file names and URLs, so anything odd is refused).
pub fn normalize_tag(t: &str) -> Option<String> {
    let t = t.trim();
    let bare = t.strip_prefix('v').unwrap_or(t);
    let ok = !bare.is_empty()
        && bare.len() <= 40
        && bare.as_bytes()[0].is_ascii_digit()
        && bare.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !bare.contains("..");
    ok.then(|| format!("v{bare}"))
}

/// `(major, minor, patch)` from `v0.1.60` / `0.1.60-rc1`.
pub fn version_key(tag: &str) -> Option<(u64, u64, u64)> {
    let bare = tag.trim().trim_start_matches('v');
    let core = bare.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next().unwrap_or("0").parse().ok()?;
    let c = it.next().unwrap_or("0").parse().ok()?;
    Some((a, b, c))
}

pub fn version_gt(a: &str, b: &str) -> bool {
    match (version_key(a), version_key(b)) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

// ---- releases ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub tag: String,
    pub published_at: Option<u64>,
    pub html_url: Option<String>,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// The newest published release (highest version; ties go to the first listed).
pub fn latest(releases: &[Release]) -> Option<&Release> {
    let mut best: Option<&Release> = None;
    for r in releases {
        match best {
            None => best = Some(r),
            Some(b) if version_gt(&r.tag, &b.tag) => best = Some(r),
            _ => {}
        }
    }
    best
}

/// `2026-10-12T03:33:10Z` -> unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + se).ok()
}

/// Parse GitHub's list (or a single release object). Drafts and pre-releases are skipped.
pub fn parse_releases(body: &[u8]) -> Result<Vec<Release>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "GitHub sent something that is not JSON.".to_string())?;
    let items: Vec<Value> = match v {
        Value::Array(a) => a,
        o @ Value::Object(_) if o.get("tag_name").is_some() => vec![o],
        Value::Object(o) => {
            let msg = o.get("message").and_then(Value::as_str).unwrap_or("unexpected answer");
            return Err(format!("GitHub said: {msg}"));
        }
        _ => return Err("GitHub sent an unexpected answer.".into()),
    };
    let mut out = Vec::new();
    for it in items {
        if it.get("draft").and_then(Value::as_bool) == Some(true)
            || it.get("prerelease").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(tag) = it.get("tag_name").and_then(Value::as_str).and_then(normalize_tag) else { continue };
        let assets = it
            .get("assets")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        Some(Asset {
                            name: x.get("name")?.as_str()?.to_string(),
                            size: x.get("size").and_then(Value::as_u64).unwrap_or(0),
                            url: x.get("browser_download_url")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(Release {
            tag,
            published_at: it.get("published_at").and_then(Value::as_str).and_then(parse_rfc3339),
            html_url: it.get("html_url").and_then(Value::as_str).map(str::to_string),
            assets,
        });
    }
    Ok(out)
}

/// The answer of [`Cache::get`].
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub releases: Vec<Release>,
    /// Unix seconds of the check this comes from.
    pub checked_at: Option<u64>,
    /// GitHub could not be reached just now; `releases` is the last answer (maybe none).
    pub stale: bool,
    pub error: Option<String>,
}

#[derive(Default)]
struct CacheInner {
    releases: Vec<Release>,
    checked_unix: Option<u64>,
    checked: Option<Instant>,
    last_error: Option<String>,
    last_force: Option<Instant>,
    /// After a failed check, no new attempt before this (a minute), so a dead network is not hammered.
    retry_after: Option<Instant>,
}

#[derive(Serialize, Deserialize)]
struct DiskCache {
    checked_at: u64,
    releases: Vec<Release>,
}

pub struct Cache {
    source: Source,
    dir: Mutex<Option<PathBuf>>,
    inner: Mutex<CacheInner>,
    client: OnceLock<Result<reqwest::Client, String>>,
}

impl Cache {
    pub fn new(source: Source) -> Arc<Self> {
        Arc::new(Self {
            source,
            dir: Mutex::new(None),
            inner: Mutex::new(CacheInner::default()),
            client: OnceLock::new(),
        })
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    /// Where `releases.json` lives. Loads an earlier answer from there, so an offline start still has one.
    pub fn set_dir(&self, dir: Option<PathBuf>) {
        *self.dir.lock().unwrap() = dir.clone();
        let Some(dir) = dir else { return };
        let Ok(bytes) = std::fs::read(dir.join("releases.json")) else { return };
        let Ok(d) = serde_json::from_slice::<DiskCache>(&bytes) else { return };
        let mut i = self.inner.lock().unwrap();
        if i.checked.is_none() && i.releases.is_empty() {
            i.releases = d.releases;
            i.checked_unix = Some(d.checked_at);
        }
    }

    fn client(&self) -> Result<reqwest::Client, String> {
        self.client.get_or_init(|| self.source.client()).clone()
    }

    /// Newest release tag known, without any network.
    pub fn peek_latest(&self) -> Option<String> {
        latest(&self.inner.lock().unwrap().releases).map(|r| r.tag.clone())
    }

    /// The cache is empty or older than its lifetime, and no retry is being held back.
    pub fn needs_check(&self) -> bool {
        let i = self.inner.lock().unwrap();
        let stale = i.checked.is_none_or(|t| t.elapsed() >= CACHE_TTL);
        stale && i.retry_after.is_none_or(|t| Instant::now() >= t)
    }

    /// The last answer, without any network (what the status shows).
    pub fn peek(&self) -> Fetched {
        let i = self.inner.lock().unwrap();
        Fetched {
            releases: i.releases.clone(),
            checked_at: i.checked_unix,
            stale: i.checked.is_none_or(|t| t.elapsed() > CACHE_TTL),
            error: i.last_error.clone(),
        }
    }

    /// The releases, from the cache while it is fresh (ten minutes). `force` asks GitHub now, at most once a
    /// minute.
    pub async fn get(&self, force: bool) -> Fetched {
        {
            let mut i = self.inner.lock().unwrap();
            let fresh = i.checked.is_some_and(|t| t.elapsed() < CACHE_TTL);
            let may_force = i.last_force.is_none_or(|t| t.elapsed() >= MIN_FORCE_GAP);
            if fresh && !(force && may_force) {
                return Fetched {
                    releases: i.releases.clone(),
                    checked_at: i.checked_unix,
                    stale: false,
                    error: None,
                };
            }
            if i.retry_after.is_some_and(|t| Instant::now() < t) && !(force && may_force) {
                return Fetched {
                    releases: i.releases.clone(),
                    checked_at: i.checked_unix,
                    stale: true,
                    error: i.last_error.clone(),
                };
            }
            if force && may_force {
                i.last_force = Some(Instant::now());
            }
        }
        match self.fetch_list().await {
            Ok(list) => {
                let now = crate::persist::unix_now();
                {
                    let mut i = self.inner.lock().unwrap();
                    i.releases = list.clone();
                    i.checked_unix = Some(now);
                    i.checked = Some(Instant::now());
                    i.last_error = None;
                    i.retry_after = None;
                }
                self.save(now, &list);
                Fetched { releases: list, checked_at: Some(now), stale: false, error: None }
            }
            Err(e) => {
                let mut i = self.inner.lock().unwrap();
                i.last_error = Some(e.clone());
                i.retry_after = Some(Instant::now() + MIN_FORCE_GAP);
                Fetched { releases: i.releases.clone(), checked_at: i.checked_unix, stale: true, error: Some(e) }
            }
        }
    }

    /// One release by tag: from the list when it is there, else from GitHub.
    pub async fn release(&self, tag: &str) -> Result<Release, String> {
        let known = self.get(false).await;
        if let Some(r) = known.releases.iter().find(|r| r.tag == tag) {
            return Ok(r.clone());
        }
        let url = format!("{}/tags/{tag}", self.source.api_url.trim_end_matches('/'));
        let body = self.get_body(&url).await?;
        let mut list = parse_releases(&body)?;
        list.retain(|r| r.tag == tag);
        list.pop().ok_or_else(|| format!("{NOT_FOUND_PREFIX} {tag}."))
    }

    async fn fetch_list(&self) -> Result<Vec<Release>, String> {
        let url = format!("{}?per_page=10", self.source.api_url.trim_end_matches('/'));
        parse_releases(&self.get_body(&url).await?)
    }

    async fn get_body(&self, url: &str) -> Result<Vec<u8>, String> {
        let client = self.client()?;
        let mut resp = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(net_error)?;
        let code = resp.status();
        if code == reqwest::StatusCode::FORBIDDEN || code == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err("GitHub is limiting requests from this address. Try again in a while.".into());
        }
        if code == reqwest::StatusCode::NOT_FOUND {
            return Err(NOT_FOUND_PREFIX.to_string() + ".");
        }
        if !code.is_success() {
            return Err(format!("GitHub answered HTTP {}.", code.as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(net_error)? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_API_BYTES {
                return Err("GitHub's answer was unexpectedly large.".into());
            }
        }
        Ok(body)
    }

    fn save(&self, checked_at: u64, list: &[Release]) {
        let Some(dir) = self.dir.lock().unwrap().clone() else { return };
        let d = DiskCache { checked_at, releases: list.to_vec() };
        if let Err(e) = crate::control::write_atomic(&dir.join("releases.json"), &d) {
            tracing::debug!("releases: cannot save releases.json: {e}");
        }
    }
}

/// A network failure in plain words.
pub fn net_error(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "GitHub did not answer in time.".into()
    } else if e.is_connect() {
        "Could not reach GitHub. Check the internet connection.".into()
    } else if e.is_redirect() {
        "GitHub sent the download somewhere that is not allowed.".into()
    } else {
        format!("The download failed: {}", e.without_url())
    }
}

// ---- bin/installed.json -----------------------------------------------------------------------

/// `bin/installed.json`: which official release the program in `bin/blvm` is.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InstalledRecord {
    pub tag: String,
    pub asset: String,
    pub sha256: String,
    /// Unix seconds.
    pub installed_at: u64,
    pub previous_tag: Option<String>,
}

pub fn installed_path(root: &Path) -> PathBuf {
    root.join("bin").join(INSTALLED_FILE)
}

/// The record, when there is one that names a release.
pub fn read_installed(root: &Path) -> Option<InstalledRecord> {
    let r: InstalledRecord = serde_json::from_slice(&std::fs::read(installed_path(root)).ok()?).ok()?;
    (!r.tag.is_empty()).then_some(r)
}

pub fn write_installed(root: &Path, r: &InstalledRecord) -> std::io::Result<()> {
    crate::control::write_atomic(&installed_path(root), r)
}

// ---- the answer of GET /api/node/releases ------------------------------------------------------

/// Everything the releases endpoint needs to decide `can_install` and `available`.
pub struct InfoInputs<'a> {
    pub managed: bool,
    /// `self` | `store` | `off`.
    pub channel: &'a str,
    pub platform: &'a str,
    /// The record, when the program in use is the installer's.
    pub installed: Option<&'a InstalledRecord>,
    /// The program in use did not come from the installer (or nothing is installed).
    pub custom: bool,
    /// `BLVM_UI_NODE_BIN` pins the program: the installer would write somewhere the console does not look.
    pub bin_pinned_by_env: bool,
    pub fetched: &'a Fetched,
}

/// Why the installer cannot be used here, whatever GitHub says; `None` when nothing is in the way.
///
/// The first install is always allowed (an app store install has no other way to get its node); once a
/// release is installed, updates follow the channel (`self` only).
pub fn static_block(managed: bool, channel: &str, platform: &str, bin_pinned_by_env: bool, installed: bool) -> Option<String> {
    if !managed {
        return Some("This console is not managing a node. Run the setup and choose \"Run a node for me\".".into());
    }
    if installed {
        match channel {
            "off" => return Some("Updates from inside the console are turned off.".into()),
            "store" => return Some("Update this app from the Umbrel App Store.".into()),
            _ => {}
        }
    }
    if platform == PLATFORM_UNSUPPORTED {
        return Some(format!(
            "There is no official node build for this computer ({} {}). Official builds exist for Linux (x86_64 and aarch64).",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    }
    if bin_pinned_by_env {
        return Some("The node program is set by BLVM_UI_NODE_BIN, so the console will not replace it.".into());
    }
    None
}

/// [`static_block`], plus what the release list says.
pub fn install_block(i: &InfoInputs<'_>, latest_asset_present: bool) -> Option<String> {
    if let Some(b) = static_block(i.managed, i.channel, i.platform, i.bin_pinned_by_env, i.installed.is_some() && !i.custom) {
        return Some(b);
    }
    if i.fetched.releases.is_empty() {
        return Some(match &i.fetched.error {
            Some(e) => e.clone(),
            None => "Could not check for releases.".into(),
        });
    }
    if !latest_asset_present {
        return Some(format!("The newest release has no build for {}.", i.platform));
    }
    None
}

/// The installer's program is in place (`bin/blvm` exists).
pub fn installed_bin_ok(root: &Path) -> bool {
    crate::settings::installed_bin(root).is_file()
}

/// The JSON of `GET /api/node/releases`. Keys follow design F (`current`, `custom`, `latest`, `available`,
/// `platform_supported`, `checked_at`, `notes_url`, `channel`) and the page's `ReleaseInfo` (`installed`,
/// `latest{tag,published_at,notes_url}`, `asset{name,size,platform}`, `platform`, `can_install`, `reason`).
pub fn info_json(i: &InfoInputs<'_>) -> Value {
    let latest_rel = latest(&i.fetched.releases);
    let asset = latest_rel.and_then(|r| {
        (i.platform != PLATFORM_UNSUPPORTED).then(|| r.asset(&asset_name(&r.tag, i.platform))).flatten()
    });
    let reason = install_block(i, asset.is_some());
    let available = match (i.installed, latest_rel) {
        (Some(rec), Some(l)) if !i.custom => version_gt(&l.tag, &rec.tag),
        _ => false,
    };
    let notes_url = latest_rel.and_then(|r| r.html_url.clone());
    let offline_reason = if i.fetched.releases.is_empty() { i.fetched.error.clone() } else { None };
    json!({
        "installed": i.installed.filter(|_| !i.custom).map(|r| json!({"tag": r.tag, "installed_at": r.installed_at})),
        "latest": latest_rel.map(|r| json!({"tag": r.tag, "published_at": r.published_at, "notes_url": r.html_url})),
        "asset": asset.map(|a| json!({"name": a.name, "size": a.size, "platform": i.platform})),
        "platform": i.platform,
        "can_install": reason.is_none(),
        "reason": reason,
        "available": available,
        "channel": i.channel,
        // design F names
        "current": i.installed.filter(|_| !i.custom).map(|r| r.tag.clone()),
        "custom": i.custom,
        "platform_supported": i.platform != PLATFORM_UNSUPPORTED,
        "checked_at": i.fetched.checked_at,
        "notes_url": notes_url,
        // where the answer came from
        "stale": i.fetched.stale,
        "offline": i.fetched.error.is_some() && i.fetched.stale,
        "check_error": i.fetched.error,
        "unavailable_reason": offline_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      {"tag_name":"v0.1.60","draft":false,"prerelease":false,"published_at":"2026-10-12T03:33:10Z",
       "html_url":"https://github.com/BTCDecoded/blvm/releases/tag/v0.1.60",
       "assets":[{"name":"blvm-0.1.60-linux-x86_64","size":50067352,"browser_download_url":"https://github.com/BTCDecoded/blvm/releases/download/v0.1.60/blvm-0.1.60-linux-x86_64"},
                 {"name":"checksums.sha256","size":1502,"browser_download_url":"https://github.com/BTCDecoded/blvm/releases/download/v0.1.60/checksums.sha256"}]},
      {"tag_name":"v0.1.61-rc1","draft":false,"prerelease":true,"assets":[]},
      {"tag_name":"v0.1.62","draft":true,"prerelease":false,"assets":[]},
      {"tag_name":"v0.1.59","draft":false,"prerelease":false,"assets":[]},
      {"tag_name":"../evil","draft":false,"prerelease":false,"assets":[]}
    ]"#;

    #[test]
    fn github_json_is_parsed_and_drafts_prereleases_and_odd_tags_are_dropped() {
        let r = parse_releases(SAMPLE.as_bytes()).unwrap();
        assert_eq!(r.iter().map(|r| r.tag.as_str()).collect::<Vec<_>>(), vec!["v0.1.60", "v0.1.59"]);
        assert_eq!(r[0].published_at, Some(1_791_775_990));
        assert_eq!(r[0].assets.len(), 2);
        assert_eq!(r[0].asset("blvm-0.1.60-linux-x86_64").unwrap().size, 50_067_352);
        assert_eq!(latest(&r).unwrap().tag, "v0.1.60");
        assert!(parse_releases(br#"{"message":"API rate limit exceeded"}"#).unwrap_err().contains("rate limit"));
        assert!(parse_releases(b"<html>").is_err());
    }

    #[test]
    fn timestamps_and_versions() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-12T03:33:10Z"), Some(1_791_775_990));
        assert_eq!(parse_rfc3339("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(parse_rfc3339("nope"), None);
        assert!(version_gt("v0.1.61", "v0.1.60") && version_gt("v0.2.0", "v0.1.99") && version_gt("v1.0.0", "0.9.9"));
        assert!(!version_gt("v0.1.60", "v0.1.60") && !version_gt("v0.1.59", "v0.1.60"));
        assert!(!version_gt("garbage", "v0.1.60"));
        assert_eq!(normalize_tag("0.1.60").as_deref(), Some("v0.1.60"));
        assert_eq!(normalize_tag(" v0.1.60 ").as_deref(), Some("v0.1.60"));
        for bad in ["", "v", "latest", "v../x", "0.1.60/../../etc", "v0.1.60 ; rm", "0..1"] {
            assert_eq!(normalize_tag(bad), None, "{bad:?}");
        }
        assert_eq!(asset_name("v0.1.60", "linux-aarch64"), "blvm-0.1.60-linux-aarch64");
    }

    #[test]
    fn the_update_flag_needs_a_release_install_and_a_newer_tag() {
        let fetched = Fetched {
            releases: parse_releases(SAMPLE.as_bytes()).unwrap(),
            checked_at: Some(5),
            stale: false,
            error: None,
        };
        let old = InstalledRecord { tag: "v0.1.59".into(), installed_at: 7, ..Default::default() };
        let mut inp = InfoInputs {
            managed: true,
            channel: "self",
            platform: "linux-x86_64",
            installed: Some(&old),
            custom: false,
            bin_pinned_by_env: false,
            fetched: &fetched,
        };
        let v = info_json(&inp);
        assert_eq!(v["available"], true);
        assert_eq!(v["can_install"], true);
        assert_eq!(v["latest"]["tag"], "v0.1.60");
        assert_eq!(v["asset"]["name"], "blvm-0.1.60-linux-x86_64");
        assert_eq!(v["asset"]["platform"], "linux-x86_64");
        assert_eq!(v["installed"]["tag"], "v0.1.59");
        assert_eq!(v["current"], "v0.1.59");
        assert_eq!(v["notes_url"], "https://github.com/BTCDecoded/blvm/releases/tag/v0.1.60");

        // A custom build never gets the badge.
        inp.custom = true;
        let v = info_json(&inp);
        assert_eq!((v["available"].clone(), v["installed"].clone(), v["custom"].clone()), (json!(false), Value::Null, json!(true)));

        // No asset for the platform, no platform at all, the store channel, updates off, env pin.
        inp.custom = false;
        inp.platform = "linux-aarch64";
        assert!(info_json(&inp)["reason"].as_str().unwrap().contains("no build for linux-aarch64"));
        inp.platform = "unsupported";
        let v = info_json(&inp);
        assert_eq!((v["can_install"].clone(), v["platform_supported"].clone(), v["asset"].clone()), (json!(false), json!(false), Value::Null));
        inp.platform = "linux-x86_64";
        inp.channel = "store";
        assert!(info_json(&inp)["reason"].as_str().unwrap().contains("App Store"));
        inp.channel = "off";
        assert!(info_json(&inp)["reason"].as_str().unwrap().contains("turned off"));
        inp.channel = "self";
        inp.bin_pinned_by_env = true;
        assert!(info_json(&inp)["reason"].as_str().unwrap().contains("BLVM_UI_NODE_BIN"));
        inp.bin_pinned_by_env = false;
        inp.managed = false;
        assert_eq!(info_json(&inp)["can_install"], false);
    }

    #[test]
    fn offline_with_nothing_cached_says_why_and_offers_nothing() {
        let fetched = Fetched { releases: vec![], checked_at: None, stale: true, error: Some("Could not reach GitHub. Check the internet connection.".into()) };
        let inp = InfoInputs {
            managed: true,
            channel: "self",
            platform: "linux-x86_64",
            installed: None,
            custom: true,
            bin_pinned_by_env: false,
            fetched: &fetched,
        };
        let v = info_json(&inp);
        assert_eq!(v["available"], false);
        assert_eq!(v["can_install"], false);
        assert_eq!(v["latest"], Value::Null);
        assert_eq!(v["offline"], true);
        assert!(v["reason"].as_str().unwrap().contains("Could not reach GitHub"));
    }

    #[test]
    fn installed_record_round_trips() {
        let root = std::env::temp_dir().join(format!("blvm-releases-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(read_installed(&root), None);
        let r = InstalledRecord {
            tag: "v0.1.60".into(),
            asset: "blvm-0.1.60-linux-x86_64".into(),
            sha256: "ab".repeat(32),
            installed_at: 9,
            previous_tag: Some("v0.1.59".into()),
        };
        write_installed(&root, &r).unwrap();
        assert_eq!(read_installed(&root), Some(r));
        std::fs::write(installed_path(&root), "{}").unwrap();
        assert_eq!(read_installed(&root), None, "a record without a tag is no record");
        let _ = std::fs::remove_dir_all(&root);
    }
}


#[cfg(test)]
mod review_gaps {
    use super::*;

    /// REVIEW GAP: `download_url_ok` is a string prefix check; the URL parser later resolves `%2e%2e` segments,
    /// so a URL that passes the check can point outside `BTCDecoded/blvm`. (Expected to FAIL until fixed.)
    #[test]
    fn review_percent_encoded_dot_segments_do_not_escape_the_release_prefix() {
        let s = Source::default();
        let sneaky = "https://github.com/BTCDecoded/blvm/releases/download/%2e%2e/%2e%2e/%2e%2e/%2e%2e/attacker/repo/releases/download/v1/blvm";
        let parsed = reqwest::Url::parse(sneaky).unwrap();
        eprintln!("parsed path = {}", parsed.path());
        let resolves_inside = parsed.path().starts_with("/BTCDecoded/blvm/releases/download/");
        assert!(resolves_inside || !s.download_url_ok(sneaky), "accepted {sneaky}, which resolves to {}", parsed.path());
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    const GOOD: &str = "https://github.com/BTCDecoded/blvm/releases/download/v0.1.60/blvm-0.1.60-linux-x86_64";

    #[test]
    fn only_release_downloads_of_the_official_repository_are_trusted() {
        let s = Source::default();
        assert!(s.download_url_ok(GOOD));
        // Another repository, host, scheme, port, login, query, fragment.
        for bad in [
            "https://github.com/attacker/blvm/releases/download/v1/blvm",
            "https://github.com/BTCDecoded/other/releases/download/v1/blvm",
            "https://github.com.evil.example/BTCDecoded/blvm/releases/download/v1/blvm",
            "http://github.com/BTCDecoded/blvm/releases/download/v1/blvm",
            "https://github.com:8443/BTCDecoded/blvm/releases/download/v1/blvm",
            "https://BTCDecoded@github.com/BTCDecoded/blvm/releases/download/v1/blvm",
            "https://github.com@evil.example/BTCDecoded/blvm/releases/download/v1/blvm",
            "https://github.com/BTCDecoded/blvm/releases/download/v1/blvm?x=1",
            "https://github.com/BTCDecoded/blvm/releases/download/v1/blvm#frag",
            "https://github.com/BTCDecoded/blvm/releases/download/",
            "https://githubusercontent.com/BTCDecoded/blvm/releases/download/v1/blvm",
            "not a url",
        ] {
            assert!(!s.download_url_ok(bad), "accepted {bad}");
        }
    }

    #[test]
    fn dot_segments_are_refused_however_they_are_written() {
        let s = Source::default();
        for bad in [
            "https://github.com/BTCDecoded/blvm/releases/download/%2e%2e/%2e%2e/%2e%2e/%2e%2e/attacker/repo/releases/download/v1/blvm",
            "https://github.com/BTCDecoded/blvm/releases/download/%2E%2E/x/blvm",
            "https://github.com/BTCDecoded/blvm/releases/download/v1/../../../../x/y",
            "https://github.com/BTCDecoded/blvm/releases/download/v1/%2e/blvm",
            "https://github.com/BTCDecoded/blvm/releases/download/v1%2f..%2f..%2fx/blvm",
            "https://github.com/BTCDecoded/blvm/releases/download/v1%5c..%5cx/blvm",
            "https://github.com/BTCDecoded/blvm/releases/download/v1/%zz",
        ] {
            assert!(!s.download_url_ok(bad), "accepted {bad}");
        }
    }

    #[test]
    fn a_redirect_that_stays_on_github_must_stay_in_the_release_downloads() {
        let s = Source::default();
        let prefix = reqwest::Url::parse(&s.download_prefix).unwrap();
        let inside = reqwest::Url::parse("https://github.com/BTCDecoded/blvm/releases/download/v1/blvm").unwrap();
        let outside = reqwest::Url::parse("https://github.com/attacker/repo/releases/download/v1/blvm").unwrap();
        assert!(path_under(&inside, &prefix));
        assert!(!path_under(&outside, &prefix));
        // A %2e%2e hop is normalised by the parser before the policy sees it.
        let dotted = reqwest::Url::parse("https://github.com/BTCDecoded/blvm/releases/download/%2e%2e/%2e%2e/%2e%2e/%2e%2e/a/b").unwrap();
        assert!(!path_under(&dotted, &prefix));
        // The CDN host is judged by its name only.
        assert!(s.host_ok(&reqwest::Url::parse("https://objects.githubusercontent.com/github-production-release-asset/abc").unwrap()));
        assert!(!s.host_ok(&reqwest::Url::parse("https://evil.example/x").unwrap()));
    }

    #[test]
    fn percent_decoding_handles_good_and_broken_escapes() {
        assert_eq!(percent_decode("a%2Eb").as_deref(), Some("a.b"));
        assert_eq!(percent_decode("%e2%82%ac").as_deref(), Some("\u{20ac}"));
        assert_eq!(percent_decode("%ff"), None, "not UTF-8");
        assert_eq!(percent_decode("%4"), None);
        assert_eq!(percent_decode("%zz"), None);
    }
}
