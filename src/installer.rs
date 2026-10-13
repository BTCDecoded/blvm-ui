//! Install and update the official node program from GitHub releases (managed-node design D).
//!
//! `POST /api/node/install` and `POST /api/node/update` (`{"tag"?: "v0.1.60"}`, default the newest release)
//! answer 202 and run one background job at a time (a second request is 409). The job moves through
//!
//! 1. `download`: the raw asset `blvm-<version>-<platform>` into `<root>/bin/staging/<tag>/` (byte progress,
//!    at most 120 MB, only from the release download URLs, redirects only to GitHub hosts);
//! 2. `verify`: SHA-256 against the release's `checksums.sha256` (or the per-platform `SHA256SUMS-blvm-*`
//!    file); a mismatch deletes the staging folder and stops;
//! 3. `test`: the staged program is run once (`--version`, then `version`) and must exit 0 and print a version.
//!    A wrong CPU or a too-old glibc shows up here, with the system's own words;
//! 4. `install`: the file moves to `bin/blvm-<tag>`, the link `bin/blvm` is swapped to it atomically,
//!    `bin/installed.json` is written (`tag, asset, sha256, installed_at, previous_tag`), and all but the
//!    last two versions are removed;
//! 5. `restart` (only when a node was running): stop it, start it on the new program. The node must answer
//!    its RPC; if it crashes (also within [`Timings::watch`] after it first answered) or never answers, the
//!    link is put back, the node is started on the old program, and the job ends `failed` with
//!    `rolled_back: true`;
//! 6. `done`, or `failed` with `error` in plain words.
//!
//! The same job works whether the node runs in this process or under the agent in another container: the
//! program goes into the shared `<root>/bin/` and the node is restarted through the same `Agent` calls.

use crate::agent::{parse_version_output, Agent, ProcState};
use crate::releases::{self, asset_name, net_error, InstalledRecord, Release};
use crate::state::{stamp_power, Shared};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

/// Largest download accepted (the node is about 50 MB).
pub const MAX_DOWNLOAD: u64 = 120 * 1024 * 1024;
const MAX_CHECKSUM_FILE: u64 = 1024 * 1024;
const KEEP_VERSIONS: usize = 2;
/// A finished job stays in `managed.update` this long; `GET /api/node/install` keeps it until the next job.
const STATUS_LINGER: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Install,
    Update,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Install => "install",
            Kind::Update => "update",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Timings {
    /// `--version` of the staged program must finish in this time.
    pub test_timeout: Duration,
    /// After the new node first answers, a crash this soon rolls back.
    pub watch: Duration,
    /// A download that sends nothing for this long is given up.
    pub idle: Duration,
    /// A download that is still running after this long is given up, however steadily it trickles.
    pub total: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            test_timeout: Duration::from_secs(10),
            watch: Duration::from_secs(60),
            idle: Duration::from_secs(60),
            total: Duration::from_secs(15 * 60),
        }
    }
}

/// The job as the page sees it (`managed.update` and the `job` of the install/update routes).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Job {
    #[serde(skip)]
    pub id: u64,
    /// `install` | `update`; `None` before any job.
    pub kind: Option<&'static str>,
    /// `idle | download | verify | test | install | restart | done | failed`.
    pub phase: &'static str,
    pub tag: Option<String>,
    pub bytes: u64,
    pub total: u64,
    pub error: Option<String>,
    pub rolled_back: bool,
    pub started: Option<u64>,
    pub finished: Option<u64>,
}

impl Job {
    pub fn idle() -> Self {
        Self {
            id: 0,
            kind: None,
            phase: "idle",
            tag: None,
            bytes: 0,
            total: 0,
            error: None,
            rolled_back: false,
            started: None,
            finished: None,
        }
    }

    /// A job is working (a new one must wait).
    pub fn active(&self) -> bool {
        matches!(self.phase, "download" | "verify" | "test" | "install" | "restart")
    }

    /// What the status shows now: a finished job fades back to `idle` after a while.
    pub fn shown(&self, now: u64) -> Job {
        let faded = match (self.phase, self.finished) {
            ("done" | "failed", Some(f)) => now.saturating_sub(f) > STATUS_LINGER,
            _ => false,
        };
        if faded {
            Job::idle()
        } else {
            self.clone()
        }
    }
}

pub type JobCell = Arc<Mutex<Job>>;

pub fn new_cell() -> JobCell {
    Arc::new(Mutex::new(Job::idle()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn with_job(cell: &JobCell, id: u64, f: impl FnOnce(&mut Job)) {
    let mut j = cell.lock().unwrap();
    if j.id == id {
        f(&mut j);
    }
}

fn set_phase(cell: &JobCell, id: u64, phase: &'static str) {
    with_job(cell, id, |j| j.phase = phase);
}

struct Fail {
    error: String,
    rolled_back: bool,
}

impl Fail {
    fn plain(e: impl Into<String>) -> Self {
        Self { error: e.into(), rolled_back: false }
    }
}

impl From<String> for Fail {
    fn from(e: String) -> Self {
        Fail::plain(e)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The hash for `name` in a `sha256sum` style file: `<64 hex>  <name>` (a `*` before the name marks binary mode).
pub fn find_hash(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let l = l.trim();
        let (h, rest) = l.split_once(char::is_whitespace)?;
        let file = rest.trim().trim_start_matches('*');
        (file == name && h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then(|| h.to_ascii_lowercase())
    })
}

// ---- starting a job -----------------------------------------------------------------------------

/// 409 is only for "a job is running" (the page then follows that job).
fn busy() -> (u16, Value) {
    (
        409,
        json!({ "ok": false, "error": "busy", "reason": "An install or update is already running. Wait for it to finish." }),
    )
}

fn refuse(code: u16, tag: &str, msg: impl Into<String>) -> (u16, Value) {
    (code, json!({ "ok": false, "code": tag, "error": msg.into() }))
}

/// `GET /api/node/install` and `/api/node/update`: the last job, kept until a new one starts. The job
/// fields are also at the top level (`phase`, `tag`, `bytes`, `total`, `error`, `rolled_back`).
pub async fn job_json(state: &Shared) -> Value {
    let cell = state.read().await.console.job.clone();
    let j = cell.lock().unwrap().clone();
    let mut v = serde_json::to_value(&j).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("ok".into(), json!(true));
        o.insert("job".into(), serde_json::to_value(&j).unwrap_or(Value::Null));
    }
    v
}

/// Validate, claim the job slot and start the job. 202 with the job, or a refusal.
pub async fn begin(state: &Shared, kind: Kind, tag_in: Option<&str>) -> (u16, Value) {
    begin_with(state, kind, tag_in, false).await
}

/// [`begin`], where `downgrade` allows an explicit tag that is older than the installed release.
pub async fn begin_with(state: &Shared, kind: Kind, tag_in: Option<&str>, downgrade: bool) -> (u16, Value) {
    let (agent, channel, platform, root, cell, cache, timings, pinned, switching, managed) = {
        let g = state.read().await;
        let c = &g.console;
        (
            c.agent.clone(),
            c.updates_channel(),
            c.platform(),
            c.eff.data_root.clone(),
            c.job.clone(),
            c.releases.clone(),
            c.installer_timings.clone(),
            c.env.node_bin.is_some(),
            c.switch_active(),
            c.is_managed(),
        )
    };
    let Some(agent) = agent.filter(|_| managed) else {
        return refuse(400, "not_managed", "This console is not managing a node.");
    };
    if cell.lock().unwrap().active() {
        return busy();
    }
    if switching {
        return refuse(400, "switching", "A network switch is running. Wait for it to finish.");
    }
    let installed = releases::read_installed(&root).filter(|_| releases::installed_bin_ok(&root));
    if let Some(why) = releases::static_block(true, &channel, &platform, pinned, installed.is_some()) {
        return refuse(400, "unavailable", why);
    }
    if kind == Kind::Update && installed.is_none() {
        return refuse(400, "nothing_installed", "No node has been installed by the console yet. Install one first.");
    }

    // Which release.
    let tag = match tag_in.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => match releases::normalize_tag(t) {
            Some(t) => t,
            None => return refuse(400, "bad_tag", format!("\"{t}\" is not a release number such as v0.1.60.")),
        },
        None => {
            let fetched = cache.get(false).await;
            match releases::latest(&fetched.releases) {
                Some(r) => r.tag.clone(),
                None => {
                    let why = fetched.error.unwrap_or_else(|| "Could not check for releases.".into());
                    return refuse(400, "offline", why);
                }
            }
        }
    };
    if let Some(rec) = &installed {
        if rec.tag == tag {
            let msg = if tag_in.is_none() && kind == Kind::Update {
                format!("You already have the newest release ({tag}).")
            } else {
                format!("{tag} is already installed.")
            };
            return refuse(400, "up_to_date", msg);
        }
        if tag_in.is_some() && !downgrade && releases::version_gt(&rec.tag, &tag) {
            return refuse(
                400,
                "downgrade",
                format!("{tag} is older than the installed {}. Send downgrade: true to go back on purpose.", rec.tag),
            );
        }
    }
    // A release number GitHub does not know is the caller's mistake, not a job that fails a moment later.
    if tag_in.is_some() {
        if let Err(why) = cache.release(&tag).await {
            return if releases::is_not_found(&why) {
                refuse(400, "no_such_release", format!("There is no release {tag}."))
            } else {
                refuse(400, "offline", why)
            };
        }
    }

    // Claim the slot (a second request in the meantime sees `busy`).
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    {
        let mut j = cell.lock().unwrap();
        if j.active() {
            return busy();
        }
        *j = Job {
            id,
            kind: Some(kind.as_str()),
            phase: "download",
            tag: Some(tag.clone()),
            started: Some(crate::persist::unix_now()),
            ..Job::idle()
        };
    }
    let job = cell.lock().unwrap().clone();
    let st = Arc::clone(state);
    let ctx = Ctx { state: st, cell, id, tag, root, platform, cache, agent, timings };
    tokio::spawn(async move { ctx.run().await });
    {
        let mut g = state.write().await;
        stamp_power(&mut g);
    }
    (202, json!({ "ok": true, "job": job }))
}

// ---- the job --------------------------------------------------------------------------------------

struct Ctx {
    state: Shared,
    cell: JobCell,
    id: u64,
    tag: String,
    root: PathBuf,
    platform: String,
    cache: Arc<releases::Cache>,
    agent: Agent,
    timings: Timings,
}

/// What the install step changed, so a rollback can put it back.
#[derive(Debug, Clone, Default)]
struct Swap {
    /// What `bin/blvm` pointed at before (a file name inside `bin/`), if anything.
    prev_target: Option<PathBuf>,
    prev_record: Option<InstalledRecord>,
}

impl Ctx {
    fn set(&self, phase: &'static str) {
        set_phase(&self.cell, self.id, phase);
    }

    async fn touch(&self) {
        let mut g = self.state.write().await;
        stamp_power(&mut g);
    }

    fn staging(&self) -> PathBuf {
        self.root.join("bin").join("staging").join(&self.tag)
    }

    fn cleanup_staging(&self) {
        let _ = std::fs::remove_dir_all(self.staging());
        // Drop the staging folder itself when nothing else is in it.
        let _ = std::fs::remove_dir(self.root.join("bin").join("staging"));
    }

    async fn run(self) {
        let res = self.phases().await;
        self.cleanup_staging();
        let now = crate::persist::unix_now();
        match res {
            Ok(()) => {
                tracing::info!(tag = %self.tag, "installer: done");
                with_job(&self.cell, self.id, |j| {
                    j.phase = "done";
                    j.finished = Some(now);
                });
            }
            Err(f) => {
                tracing::warn!(tag = %self.tag, error = %f.error, rolled_back = f.rolled_back, "installer: failed");
                with_job(&self.cell, self.id, |j| {
                    j.phase = "failed";
                    j.error = Some(f.error);
                    j.rolled_back = f.rolled_back;
                    j.finished = Some(now);
                });
            }
        }
        self.touch().await;
    }

    async fn phases(&self) -> Result<(), Fail> {
        // ---- download
        let release = self.cache.release(&self.tag).await.map_err(Fail::plain)?;
        let name = asset_name(&self.tag, &self.platform);
        let asset = release
            .asset(&name)
            .ok_or_else(|| Fail::plain(format!("{} has no build for {}.", self.tag, self.platform)))?
            .clone();
        if asset.size > MAX_DOWNLOAD {
            return Err(Fail::plain(format!(
                "The download is {} MB, more than the {} MB allowed, so it was not fetched.",
                asset.size / 1_000_000,
                MAX_DOWNLOAD / 1_000_000
            )));
        }
        let source = self.cache.source();
        if !source.download_url_ok(&asset.url) {
            return Err(Fail::plain("The release points at a download address the console does not trust, so it was not fetched."));
        }
        with_job(&self.cell, self.id, |j| j.total = asset.size);
        self.touch().await;
        let client = source.client()?;
        let stage = self.staging();
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage).map_err(|e| Fail::plain(format!("Cannot create {}: {e}", stage.display())))?;
        let staged = stage.join(&name);
        let (sha, got) = self.download(&client, &asset.url, &staged).await?;
        if asset.size > 0 && got != asset.size {
            return Err(Fail::plain(format!(
                "The download is incomplete ({got} of {} bytes). Nothing was installed.",
                asset.size
            )));
        }

        // ---- verify
        self.set("verify");
        self.touch().await;
        let expected = self.expected_checksum(&client, &release, &name).await?;
        if expected == sha {
            tracing::info!(tag = %self.tag, asset = %name, sha256 = %sha, "installer: checksum matches the release's published checksum");
        }
        if expected != sha {
            return Err(Fail::plain(format!(
                "The downloaded file does not match the release's checksum (expected {}..., got {}...). Nothing was installed.",
                &expected[..12],
                &sha[..12]
            )));
        }

        // ---- test
        self.set("test");
        self.touch().await;
        make_executable(&staged).map_err(|e| Fail::plain(format!("Cannot make the new program executable: {e}")))?;
        let version = test_run(&staged, self.timings.test_timeout, &self.platform).await.map_err(Fail::plain)?;
        tracing::info!(tag = %self.tag, version, "installer: staged program runs");

        // ---- install
        self.set("install");
        self.touch().await;
        let was_active = self.agent.state().is_active();
        let swap = install_files(&self.root, &self.tag, &name, &sha, &staged).map_err(Fail::plain)?;
        {
            let mut g = self.state.write().await;
            g.console.reresolve();
            stamp_power(&mut g);
        }

        // ---- restart
        if was_active {
            self.restart_on_new_program(&swap).await?;
        }
        Ok(())
    }

    async fn download(&self, client: &reqwest::Client, url: &str, dest: &Path) -> Result<(String, u64), Fail> {
        let mut resp = client.get(url).send().await.map_err(|e| Fail::plain(net_error(e)))?;
        if !resp.status().is_success() {
            return Err(Fail::plain(format!("The download failed: HTTP {}.", resp.status().as_u16())));
        }
        if let Some(len) = resp.content_length() {
            if len > MAX_DOWNLOAD {
                return Err(Fail::plain(format!(
                    "The server announced {} MB, more than the {} MB allowed.",
                    len / 1_000_000,
                    MAX_DOWNLOAD / 1_000_000
                )));
            }
            with_job(&self.cell, self.id, |j| j.total = len);
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| Fail::plain(format!("Cannot write {}: {e}", dest.display())))?;
        let mut hasher = Sha256::new();
        let mut n: u64 = 0;
        let started = Instant::now();
        loop {
            let Some(left) = self.timings.total.checked_sub(started.elapsed()) else {
                return Err(Fail::plain("The download is taking too long and was given up."));
            };
            let chunk = match tokio::time::timeout(self.timings.idle.min(left), resp.chunk()).await {
                Err(_) if started.elapsed() >= self.timings.total => {
                    return Err(Fail::plain("The download is taking too long and was given up."))
                }
                Err(_) => return Err(Fail::plain("The download stalled and was given up.")),
                Ok(Err(e)) => return Err(Fail::plain(net_error(e))),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(c))) => c,
            };
            n += chunk.len() as u64;
            if n > MAX_DOWNLOAD {
                return Err(Fail::plain(format!(
                    "The download is larger than the {} MB allowed and was stopped.",
                    MAX_DOWNLOAD / 1_000_000
                )));
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| Fail::plain(format!("Cannot write {}: {e}", dest.display())))?;
            with_job(&self.cell, self.id, |j| j.bytes = n);
        }
        file.flush().await.map_err(|e| Fail::plain(format!("Cannot write {}: {e}", dest.display())))?;
        // On the disk itself, not only in the page cache: the file is renamed into place later.
        let file = file.into_std().await;
        crate::fsutil::sync_file(&file).map_err(|e| Fail::plain(format!("Cannot write {}: {e}", dest.display())))?;
        Ok((hex(&hasher.finalize()), n))
    }

    /// The release's own hash for the downloaded file.
    async fn expected_checksum(&self, client: &reqwest::Client, release: &Release, name: &str) -> Result<String, Fail> {
        let per_platform = format!("SHA256SUMS-blvm-{}", self.platform);
        let candidates = [("checksums.sha256", name), (per_platform.as_str(), "blvm")];
        let mut last_error: Option<String> = None;
        for (file, entry) in candidates {
            let Some(a) = release.asset(file) else { continue };
            if !self.cache.source().download_url_ok(&a.url) {
                continue;
            }
            match fetch_text(client, &a.url).await {
                Ok(text) => {
                    if let Some(h) = find_hash(&text, entry) {
                        return Ok(h);
                    }
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(Fail::plain(match last_error {
            Some(e) => format!("Could not fetch the release's checksum file: {e} Nothing was installed."),
            None => format!("The release has no checksum for {name}, so it was not installed."),
        }))
    }

    /// Stop the node, start it on the new program, and make sure it works; else put the old program back.
    async fn restart_on_new_program(&self, swap: &Swap) -> Result<(), Fail> {
        self.set("restart");
        self.touch().await;
        let agent = &self.agent;
        agent.cancel_restart();
        agent.request_stop();
        if !agent.wait_inactive(agent.stop_limit()).await {
            return Err(Fail::plain(
                "The node did not stop in time. The new program is installed and will be used the next time the node starts.",
            ));
        }
        if let Err(e) = agent.start().await {
            return Err(self.rollback(swap, format!("The node would not start on {}: {e}", self.tag)).await);
        }
        let t0 = Instant::now();
        let limit = agent.start_limit() + Duration::from_secs(30);
        loop {
            match agent.state() {
                ProcState::Running => break,
                ProcState::Crashed | ProcState::Stopped => {
                    let why = self.why_it_stopped();
                    return Err(self.rollback(swap, format!("The node stopped right after it started on {}: {why}", self.tag)).await);
                }
                ProcState::Starting | ProcState::Stopping => {}
            }
            if t0.elapsed() > limit {
                return Err(self.rollback(swap, format!("The node did not answer on {} in time.", self.tag)).await);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // It answers. The job is done, but a crash soon after still puts the old program back.
        let (cell, id, watch) = (Arc::clone(&self.cell), self.id, self.timings.watch);
        let (ctx, swap) = (self.clone_for_watch(), swap.clone());
        tokio::spawn(async move {
            let t0 = Instant::now();
            while t0.elapsed() < watch {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if cell.lock().unwrap().id != id {
                    return; // a newer job took over
                }
                match ctx.agent.state() {
                    ProcState::Crashed => {
                        let why = ctx.why_it_stopped();
                        let f = ctx
                            .rollback(&swap, format!("The node crashed soon after it started on {}: {why}", ctx.tag))
                            .await;
                        let now = crate::persist::unix_now();
                        with_job(&cell, id, |j| {
                            j.phase = "failed";
                            j.error = Some(f.error);
                            j.rolled_back = f.rolled_back;
                            j.finished = Some(now);
                        });
                        ctx.touch().await;
                        return;
                    }
                    // Stopped or being stopped by the user: not our business any more.
                    ProcState::Stopped | ProcState::Stopping => return,
                    _ => {}
                }
            }
        });
        Ok(())
    }

    fn clone_for_watch(&self) -> Ctx {
        Ctx {
            state: Arc::clone(&self.state),
            cell: Arc::clone(&self.cell),
            id: self.id,
            tag: self.tag.clone(),
            root: self.root.clone(),
            platform: self.platform.clone(),
            cache: Arc::clone(&self.cache),
            agent: self.agent.clone(),
            timings: self.timings.clone(),
        }
    }

    /// The agent's reason for the exit, else the last log line.
    fn why_it_stopped(&self) -> String {
        let v = self.agent.view();
        let last = self.agent.logs(None, 5).into_iter().rev().map(|l| l.text).find(|t| !t.trim().is_empty());
        match (v.last_error, last) {
            (Some(e), Some(l)) => format!("{e} Last log line: {l}"),
            (Some(e), None) => e,
            (None, Some(l)) => format!("last log line: {l}"),
            (None, None) => "no output.".into(),
        }
    }

    /// Put the old program back, start the node on it, and describe what happened.
    async fn rollback(&self, swap: &Swap, why: String) -> Fail {
        let agent = &self.agent;
        agent.abandon_restart();
        agent.request_stop();
        agent.wait_inactive(agent.stop_limit()).await;
        if let Err(e) = restore_files(&self.root, swap) {
            tracing::warn!("installer: rollback could not restore the files: {e}");
            return Fail { error: format!("{why} The old program could not be put back: {e}"), rolled_back: false };
        }
        let has_old = {
            let mut g = self.state.write().await;
            g.console.reresolve();
            let ok = g.console.eff.node_bin.as_deref().is_some_and(Path::is_file);
            stamp_power(&mut g);
            ok
        };
        if has_old {
            if let Err(e) = agent.start().await {
                tracing::warn!("installer: rollback could not start the old program: {e}");
            }
        }
        let error = if has_old {
            format!("{why} The previous program was put back and the node was started on it.")
        } else {
            format!("{why} There is no earlier program to go back to.")
        };
        Fail { error, rolled_back: has_old }
    }
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let mut resp = client.get(url).timeout(Duration::from_secs(30)).send().await.map_err(net_error)?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}.", resp.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(c) = resp.chunk().await.map_err(net_error)? {
        body.extend_from_slice(&c);
        if body.len() as u64 > MAX_CHECKSUM_FILE {
            return Err("the checksum file is unexpectedly large.".into());
        }
    }
    String::from_utf8(body).map_err(|_| "the checksum file is not text.".into())
}

// ---- the staged program -----------------------------------------------------------------------

#[cfg(unix)]
fn make_executable(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
}
#[cfg(not(unix))]
fn make_executable(_p: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A `X.Y.Z` anywhere in the text (for programs that print their name differently).
fn find_semver(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
        .map(|t| t.trim_start_matches('v'))
        .find(|t| {
            let mut p = t.split('.');
            matches!((p.next(), p.next(), p.next()), (Some(a), Some(b), Some(c))
                if [a, b, c].iter().all(|x| !x.is_empty() && x.as_bytes()[0].is_ascii_digit()))
        })
        .map(str::to_string)
}

fn short(s: &str) -> String {
    let t: String = s.trim().chars().take(300).collect();
    if t.is_empty() {
        "no output".into()
    } else {
        t
    }
}

/// Look at the first bytes of the file: a Linux program on another system, or for another CPU, is named as
/// what it is before anything is run (some systems answer an unrunnable file with a shell's error text).
fn check_format(head: &[u8], platform: &str, os: &str, arch: &str) -> Result<(), String> {
    if head.len() < 20 || &head[..4] != b"\x7fELF" {
        return Ok(()); // not an ELF file: let running it say what is wrong
    }
    let unsupported = |why: String| {
        Err(format!(
            "Unsupported platform: this program is built for {platform} but this computer is {os} {arch}{why}. Nothing was installed."
        ))
    };
    if os != "linux" {
        return unsupported(String::new());
    }
    let machine = u16::from_le_bytes([head[18], head[19]]);
    let built_for = match machine {
        0x3e => "x86_64",
        0xb7 => "aarch64",
        _ => return Ok(()),
    };
    if built_for != arch {
        return unsupported(format!(" (the file is for {built_for})"));
    }
    Ok(())
}

/// Run the staged program once. `Ok(version)` when it exits 0 and prints a version.
async fn test_run(bin: &Path, limit: Duration, platform: &str) -> Result<String, String> {
    {
        use std::io::Read;
        let mut head = [0u8; 20];
        let n = std::fs::File::open(bin).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
        check_format(&head[..n], platform, std::env::consts::OS, std::env::consts::ARCH)?;
    }
    let mut first_problem: Option<String> = None;
    for arg in ["--version", "version"] {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.arg(arg)
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("BLVM_UI_") {
                cmd.env_remove(&k);
            }
        }
        let out = match tokio::time::timeout(limit, cmd.output()).await {
            Err(_) => return Err(format!("The new program did not answer `{arg}` in {} seconds.", limit.as_secs())),
            Ok(Err(e)) => return Err(spawn_problem(&e, platform)),
            Ok(Ok(o)) => o,
        };
        let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        if out.status.success() {
            if let Some(v) = parse_version_output(&text).or_else(|| find_semver(&text)) {
                return Ok(v);
            }
            first_problem.get_or_insert_with(|| format!("The new program ran but printed no version ({}).", short(&text)));
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = format!(
                "The new program does not run on this computer: {}",
                short(if stderr.trim().is_empty() { &text } else { &stderr })
            );
            // A library problem will not change with another argument.
            if stderr.contains("GLIBC") || stderr.contains("error while loading shared libraries") {
                return Err(format!("{msg} The official build needs Linux with glibc 2.39 or newer (Ubuntu 24.04, Debian 13)."));
            }
            first_problem.get_or_insert(msg);
        }
    }
    Err(first_problem.unwrap_or_else(|| "The new program did not report a version.".into()))
}

fn spawn_problem(e: &std::io::Error, platform: &str) -> String {
    #[cfg(unix)]
    let wrong_arch = e.raw_os_error() == Some(libc::ENOEXEC);
    #[cfg(not(unix))]
    let wrong_arch = false;
    if wrong_arch {
        format!(
            "Unsupported platform: this program is built for {platform} and cannot run on this computer ({} {}). ({e})",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    } else {
        format!(
            "The new program cannot run on this computer ({e}). The official build needs Linux with glibc 2.39 or newer (Ubuntu 24.04, Debian 13)."
        )
    }
}

// ---- files ---------------------------------------------------------------------------------------

#[cfg(unix)]
fn symlink_swap(dir: &Path, link: &str, target: &Path) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{link}.swap{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, dir.join(link)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    // The new link is only as durable as the folder entry that holds it.
    let _ = crate::fsutil::sync_dir(dir);
    Ok(())
}
#[cfg(not(unix))]
fn symlink_swap(dir: &Path, link: &str, target: &Path) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{link}.swap"));
    std::fs::copy(dir.join(target), &tmp)?;
    std::fs::rename(&tmp, dir.join(link))
}

/// Move the staged program into `bin/`, swap `bin/blvm` to it, write the record, prune old versions.
fn install_files(root: &Path, tag: &str, asset: &str, sha: &str, staged: &Path) -> Result<Swap, String> {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| format!("Cannot create {}: {e}", bin.display()))?;
    let file_name = format!("blvm-{tag}");
    // The staged file is already synced (see `download`); the folder must know the new name before a link
    // points at it.
    std::fs::rename(staged, bin.join(&file_name)).map_err(|e| format!("Cannot place the new program: {e}"))?;
    crate::fsutil::sync_dir(&bin).map_err(|e| format!("Cannot place the new program: {e}"))?;

    let link = bin.join("blvm");
    let prev_record = releases::read_installed(root);
    let prev_target = match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => std::fs::read_link(&link).ok(),
        Ok(m) if m.is_file() => {
            // A program somebody put there by hand: keep it, and be able to go back to it.
            let kept = PathBuf::from("blvm-previous-file");
            std::fs::rename(&link, bin.join(&kept)).map_err(|e| format!("Cannot keep the old program: {e}"))?;
            Some(kept)
        }
        _ => None,
    };
    symlink_swap(&bin, "blvm", Path::new(&file_name)).map_err(|e| format!("Cannot switch to the new program: {e}"))?;
    let rec = InstalledRecord {
        tag: tag.to_string(),
        asset: asset.to_string(),
        sha256: sha.to_string(),
        installed_at: crate::persist::unix_now(),
        previous_tag: prev_record.as_ref().map(|r| r.tag.clone()).filter(|t| t != tag),
    };
    releases::write_installed(root, &rec).map_err(|e| format!("Cannot write installed.json: {e}"))?;
    let swap = Swap { prev_target, prev_record };
    prune(&bin, &rec, &swap);
    Ok(swap)
}

/// Keep the current and the previous version on disk, delete the rest of `bin/blvm-v*`.
fn prune(bin: &Path, current: &InstalledRecord, swap: &Swap) {
    let mut keep: Vec<String> = vec![format!("blvm-{}", current.tag)];
    if let Some(p) = &current.previous_tag {
        keep.push(format!("blvm-{p}"));
    }
    if let Some(t) = swap.prev_target.as_ref().and_then(|t| t.file_name()) {
        keep.push(t.to_string_lossy().into_owned());
    }
    keep.truncate(KEEP_VERSIONS + 1);
    let Ok(rd) = std::fs::read_dir(bin) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("blvm-v") && !keep.contains(&name) && e.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Undo [`install_files`]: `bin/blvm` and `installed.json` as they were.
fn restore_files(root: &Path, swap: &Swap) -> Result<(), String> {
    let bin = root.join("bin");
    match &swap.prev_target {
        Some(t) => symlink_swap(&bin, "blvm", t).map_err(|e| e.to_string())?,
        None => {
            let _ = std::fs::remove_file(bin.join("blvm"));
        }
    }
    match &swap.prev_record {
        Some(r) => releases::write_installed(root, r).map_err(|e| e.to_string())?,
        None => {
            let _ = std::fs::remove_file(releases::installed_path(root));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod testkit {
    //! A tiny HTTP server standing in for GitHub, shared by the tests of `releases` and `installer`.
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Default)]
    pub struct Resp {
        pub status: u16,
        pub body: Vec<u8>,
        pub location: Option<String>,
        /// Claim the whole body in Content-Length but send only this many bytes, then hang up.
        pub cut_at: Option<usize>,
        /// Send the body in pieces of this many bytes with this many milliseconds between them.
        pub trickle: Option<(usize, u64)>,
    }

    impl Resp {
        pub fn ok(body: impl Into<Vec<u8>>) -> Self {
            Self { status: 200, body: body.into(), ..Default::default() }
        }
    }

    pub struct Fake {
        pub addr: std::net::SocketAddr,
        pub routes: Arc<Mutex<HashMap<String, Resp>>>,
        pub hits: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        pub fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }

        pub fn set(&self, path: &str, r: Resp) {
            self.routes.lock().unwrap().insert(path.to_string(), r);
        }

        pub fn count(&self, path_prefix: &str) -> usize {
            self.hits.lock().unwrap().iter().filter(|p| p.starts_with(path_prefix)).count()
        }
    }

    pub async fn start() -> Fake {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let routes: Arc<Mutex<HashMap<String, Resp>>> = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let (r2, h2) = (Arc::clone(&routes), Arc::clone(&hits));
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let (routes, hits) = (Arc::clone(&r2), Arc::clone(&h2));
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut got = 0;
                    loop {
                        let Ok(n) = s.read(&mut buf[got..]).await else { return };
                        if n == 0 {
                            return;
                        }
                        got += n;
                        if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") || got == buf.len() {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..got]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    hits.lock().unwrap().push(path.clone());
                    let key = path.split('?').next().unwrap_or("/").to_string();
                    let r = routes.lock().unwrap().get(&key).cloned();
                    let Some(r) = r else {
                        let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    };
                    let reason = match r.status {
                        200 => "OK",
                        302 => "Found",
                        403 => "Forbidden",
                        _ => "Other",
                    };
                    let loc = r.location.as_ref().map(|l| format!("Location: {l}\r\n")).unwrap_or_default();
                    let hdr = format!(
                        "HTTP/1.1 {} {reason}\r\n{loc}Content-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        r.status,
                        r.body.len()
                    );
                    let _ = s.write_all(hdr.as_bytes()).await;
                    let body = match r.cut_at {
                        Some(n) => &r.body[..n.min(r.body.len())],
                        None => &r.body[..],
                    };
                    let piece = r.trickle.map_or(16 * 1024, |(n, _)| n.max(1));
                    for part in body.chunks(piece) {
                        if s.write_all(part).await.is_err() {
                            return;
                        }
                        if let Some((_, ms)) = r.trickle {
                            let _ = s.flush().await;
                            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                        }
                    }
                    let _ = s.shutdown().await;
                });
            }
        });
        Fake { addr, routes, hits }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::testkit::{self, Fake, Resp};
    use super::*;
    use crate::agent::{Agent, Timings as AgentTimings};
    use crate::managed::Console;
    use crate::releases::{Cache, Source};
    use crate::settings::EnvOverrides;
    use crate::state::LiveState;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::RwLock;

    const VER: &str = "0.1.60";

    fn fast() -> AgentTimings {
        AgentTimings {
            start_timeout: Duration::from_secs(10),
            start_max: Duration::from_secs(30),
            owner_check: Duration::from_millis(100),
            stop_wait: Duration::from_millis(1500),
            kill_wait: Duration::from_secs(3),
            backoff: vec![Duration::from_millis(150), Duration::from_millis(250), Duration::from_millis(350)],
            crash_window: Duration::from_secs(60),
            ready_poll: Duration::from_millis(30),
            tail_poll: Duration::from_millis(30),
            log_max_bytes: 50 * 1024 * 1024,
            version_timeout: Duration::from_secs(5),
            meta_tick: Duration::from_millis(200),
            stop_give_up: None,
        }
    }

    /// A stand-in for the node: prints its version, runs until SIGTERM. `crash` makes it exit 3 `after` seconds
    /// after it starts.
    fn script(ver: &str, crash: Option<&str>) -> Vec<u8> {
        let run = match crash {
            Some(after) => format!("echo \"node v{ver} up\"; sleep {after}; echo boom; exit 3"),
            None => format!("echo \"node v{ver} up\"; while kill -0 $PPID 2>/dev/null; do sleep 0.1; done"),
        };
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ] || [ \"$1\" = \"version\" ]; then echo \"blvm {ver}\"; exit 0; fi\ntrap 'exit 0' TERM\n{run}\n"
        )
        .into_bytes()
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    struct Fx {
        dir: PathBuf,
        root: PathBuf,
        state: Shared,
        fake: Fake,
        ready: Arc<AtomicBool>,
        published: Mutex<Vec<(String, Vec<u8>)>>,
        _reap: crate::agent::testkit::ReapNodes,
    }

    impl Fx {
        fn cell(&self) -> JobCell {
            self.state.try_read().unwrap().console.job.clone()
        }

        fn agent(&self) -> Agent {
            self.state.try_read().unwrap().console.agent.clone().unwrap()
        }

        /// Make `tag` (as `v0.1.x`) downloadable with `body`, and list it at the API address.
        fn publish(&self, tag: &str, body: Vec<u8>) {
            self.publish_with(tag, body, None, None);
        }

        fn publish_with(&self, tag: &str, body: Vec<u8>, wrong_sum: Option<String>, cut_at: Option<usize>) {
            let ver = tag.trim_start_matches('v');
            let name = format!("blvm-{ver}-linux-x86_64");
            self.fake.set(&format!("/dl/{tag}/{name}"), Resp { status: 200, body: body.clone(), cut_at, ..Default::default() });
            let sum = wrong_sum.unwrap_or_else(|| sha(&body));
            self.fake.set(&format!("/dl/{tag}/checksums.sha256"), Resp::ok(format!("{sum}  {name}\n0000  other-file\n")));
            // A new release list: the console's ten-minute cache would hide it.
            if let Ok(mut g) = self.state.try_write() {
                let src = g.console.releases.source().clone();
                g.console.releases = Cache::new(src);
            }
            let mut p = self.published.lock().unwrap();
            p.retain(|(t, _)| t != tag);
            p.push((tag.to_string(), body));
            self.relist(&p, None, None);
        }

        fn relist(&self, p: &[(String, Vec<u8>)], size_of: Option<(&str, u64)>, url_base: Option<&str>) {
            let items: Vec<serde_json::Value> = p
                .iter()
                .map(|(tag, body)| {
                    let ver = tag.trim_start_matches('v');
                    let name = format!("blvm-{ver}-linux-x86_64");
                    let size = match size_of {
                        Some((t, s)) if t == tag => s,
                        _ => body.len() as u64,
                    };
                    let base = url_base.map(str::to_string).unwrap_or_else(|| self.fake.url(&format!("/dl/{tag}")));
                    json!({
                        "tag_name": tag, "draft": false, "prerelease": false, "published_at": "2026-10-12T03:33:10Z",
                        "html_url": format!("https://github.com/BTCDecoded/blvm/releases/tag/{tag}"),
                        "assets": [
                            {"name": name, "size": size, "browser_download_url": format!("{base}/{name}")},
                            {"name": "checksums.sha256", "size": 100, "browser_download_url": self.fake.url(&format!("/dl/{tag}/checksums.sha256"))},
                        ]
                    })
                })
                .collect();
            self.fake.set("/api/releases", Resp::ok(serde_json::to_vec(&items).unwrap()));
        }

        async fn begin(&self, kind: Kind, tag: Option<&str>) -> (u16, Value) {
            begin(&self.state, kind, tag).await
        }

        async fn begin_with(&self, kind: Kind, tag: Option<&str>, downgrade: bool) -> (u16, Value) {
            begin_with(&self.state, kind, tag, downgrade).await
        }

        async fn wait_job(&self, what: &str) -> Job {
            let cell = self.cell();
            for _ in 0..1000 {
                {
                    let j = cell.lock().unwrap().clone();
                    if matches!(j.phase, "done" | "failed") {
                        return j;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("timed out waiting for the job: {what}: {:?}", cell.lock().unwrap());
        }

        async fn snapshot(&self) -> crate::state::Snapshot {
            let mut g = self.state.write().await;
            stamp_power(&mut g);
            g.snapshot.clone()
        }

        fn link_target(&self) -> Option<String> {
            std::fs::read_link(self.root.join("bin/blvm")).ok().map(|p| p.display().to_string())
        }

        fn clean(&self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn fixture(tag: &str) -> Fx {
        let fake = testkit::start().await;
        let dir = std::env::temp_dir().join(format!("blvm-installer-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("root");
        let env = EnvOverrides {
            mode: Some("managed".into()),
            data_root: Some(root.display().to_string()),
            ..Default::default()
        };
        let mut console = Console::load(Some(dir.join("state")), env, None);
        console.settings.network = Some("regtest".into());
        console.settings.setup_done = true;
        console.platform_override = Some("linux-x86_64".into());
        console.releases = Cache::new(Source {
            api_url: fake.url("/api/releases"),
            download_prefix: fake.url("/dl/"),
            extra_hosts: vec!["127.0.0.1".into()],
            https_only: false,
        });
        console.installer_timings = Timings {
            test_timeout: Duration::from_secs(5),
            watch: Duration::from_secs(1),
            idle: Duration::from_secs(5),
            total: Duration::from_secs(60),
        };
        let ready = Arc::new(AtomicBool::new(true));
        let r2 = Arc::clone(&ready);
        console.agent = None;
        console.reresolve();
        let agent = Agent::with_ready(console.agent_config(), fast(), Arc::new(move || r2.load(Ordering::Relaxed)));
        agent.skip_login_check(); // the fake node has no RPC server to ask
        console.agent = Some(agent);
        let mut live = LiveState::new(console.initial_rpc());
        live.console = console;
        let _reap = crate::agent::testkit::ReapNodes::under(&dir);
        Fx { dir, root, state: Arc::new(RwLock::new(live)), fake, ready, published: Mutex::new(Vec::new()), _reap }
    }

    #[test]
    fn a_program_for_another_system_or_cpu_is_named_before_it_is_run() {
        let elf = |machine: u16| {
            let mut h = vec![0u8; 20];
            h[..4].copy_from_slice(b"\x7fELF");
            h[18..20].copy_from_slice(&machine.to_le_bytes());
            h
        };
        // Linux program on macOS.
        let e = check_format(&elf(0xb7), "linux-aarch64", "macos", "aarch64").unwrap_err();
        assert!(e.starts_with("Unsupported platform") && e.contains("macos aarch64"), "{e}");
        // Wrong CPU on Linux.
        let e = check_format(&elf(0x3e), "linux-x86_64", "linux", "aarch64").unwrap_err();
        assert!(e.contains("file is for x86_64"), "{e}");
        // Right ones, scripts, short files and unknown machines are left to the run itself.
        assert!(check_format(&elf(0x3e), "linux-x86_64", "linux", "x86_64").is_ok());
        assert!(check_format(&elf(0xb7), "linux-aarch64", "linux", "aarch64").is_ok());
        assert!(check_format(b"#!/bin/sh\necho hi\n....", "linux-x86_64", "macos", "aarch64").is_ok());
        assert!(check_format(b"short", "linux-x86_64", "macos", "aarch64").is_ok());
        assert!(check_format(&elf(0x28), "linux-x86_64", "linux", "x86_64").is_ok());
    }

    #[test]
    fn checksum_files_are_read_like_sha256sum_writes_them() {
        let h = "ab".repeat(32);
        let text = format!("{h}  blvm-0.1.60-linux-x86_64\n{}  *star-name\n\nnot a line\n", "cd".repeat(32));
        assert_eq!(find_hash(&text, "blvm-0.1.60-linux-x86_64"), Some(h));
        assert_eq!(find_hash(&text, "star-name"), Some("cd".repeat(32)));
        assert_eq!(find_hash(&text, "missing"), None);
        assert_eq!(find_hash("zz  name\n", "name"), None, "not a hash");
        assert_eq!(find_hash(&format!("{}  name", "AB".repeat(32)), "name"), Some("ab".repeat(32)), "lower-cased");
        assert_eq!(find_semver("blvm 0.1.60\nGit: x").as_deref(), Some("0.1.60"));
        assert_eq!(find_semver("hello"), None);
    }

    #[tokio::test]
    async fn install_downloads_verifies_tests_and_installs() {
        let f = fixture("ok").await;
        let body = script(VER, None);
        f.publish("v0.1.60", body.clone());

        // Before: nothing there, so no program for the console to run.
        let snap = f.snapshot().await;
        assert!(!snap.bundle_available);
        let (code, v) = f.begin(Kind::Install, None).await;
        assert_eq!(code, 202, "{v}");
        assert_eq!(v["ok"], true);
        assert_eq!(v["job"]["tag"], "v0.1.60");
        // A second request while it runs is "busy".
        let again = f.begin(Kind::Install, None).await;
        let j = f.wait_job("install").await;
        assert_eq!(j.phase, "done", "{j:?}");
        assert!(again.0 == 409 || again.0 == 400, "{again:?}");
        assert_eq!((j.bytes, j.total), (body.len() as u64, body.len() as u64));
        assert_eq!(j.kind, Some("install"));

        // Files.
        assert_eq!(f.link_target().as_deref(), Some("blvm-v0.1.60"));
        assert!(f.root.join("bin/blvm-v0.1.60").is_file());
        let rec = releases::read_installed(&f.root).unwrap();
        assert_eq!(rec.tag, "v0.1.60");
        assert_eq!(rec.asset, "blvm-0.1.60-linux-x86_64");
        assert_eq!(rec.sha256, sha(&body));
        assert_eq!(rec.previous_tag, None);
        assert!(rec.installed_at > 1_700_000_000);
        assert!(!f.root.join("bin/staging").exists(), "staging is cleaned up");

        // The console now finds the node by itself (also after a restart) and reports the release tag.
        let snap = f.snapshot().await;
        assert!(snap.bundle_available, "bundle_available turns true once installed");
        let m = snap.managed.unwrap();
        assert_eq!(m.node_version.as_deref(), Some("v0.1.60"));
        assert!(!m.custom_build);
        assert_eq!(m.installed.as_ref().map(|i| i.source), Some("release"));
        assert_eq!(m.installed.as_ref().and_then(|i| i.sha256.clone()), Some(sha(&body)));
        assert_eq!(m.update.phase, "done");
        assert_eq!(m.update.tag.as_deref(), Some("v0.1.60"));
        assert_eq!(f.agent().config().bin, Some(f.root.join("bin/blvm")));
        // The node was not running, so the install did not start it.
        assert_eq!(f.agent().state(), ProcState::Stopped);

        // And the same fresh start finds it.
        let env = EnvOverrides {
            mode: Some("managed".into()),
            data_root: Some(f.root.display().to_string()),
            ..Default::default()
        };
        let again = Console::load(Some(f.dir.join("state")), env, None);
        assert_eq!(again.eff.node_bin.as_deref(), Some(f.root.join("bin/blvm").as_path()));
        assert_eq!(again.eff.node_bin_source, "installed");
        assert_eq!(again.installed_record().map(|r| r.tag), Some("v0.1.60".into()));

        // Asking again for what is there is refused in plain words (not busy).
        let (code, v) = f.begin(Kind::Install, Some("0.1.60")).await;
        assert_eq!(code, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("already installed"));
        f.clean();
    }

    #[tokio::test]
    async fn a_wrong_checksum_installs_nothing_and_cleans_up() {
        let f = fixture("badsum").await;
        f.publish_with("v0.1.60", script(VER, None), Some("11".repeat(32)), None);
        let (code, _) = f.begin(Kind::Install, None).await;
        assert_eq!(code, 202);
        let j = f.wait_job("mismatch").await;
        assert_eq!(j.phase, "failed");
        assert!(j.error.as_deref().unwrap().contains("does not match"), "{j:?}");
        assert!(!j.rolled_back);
        assert!(!f.root.join("bin/blvm").exists() && !f.root.join("bin/blvm-v0.1.60").exists());
        assert!(!f.root.join("bin/installed.json").exists());
        assert!(!f.root.join("bin/staging").exists(), "staging deleted");
        // GET keeps the failure until the next job.
        let g = job_json(&f.state).await;
        assert_eq!((g["phase"].as_str(), g["job"]["phase"].as_str()), (Some("failed"), Some("failed")));
        assert_eq!(g["rolled_back"], false);
        f.clean();
    }

    #[tokio::test]
    async fn a_release_without_a_checksum_for_the_file_is_not_installed() {
        let f = fixture("nosum").await;
        f.publish("v0.1.60", script(VER, None));
        f.fake.set("/dl/v0.1.60/checksums.sha256", Resp::ok("1111  something-else\n"));
        f.begin(Kind::Install, None).await;
        let j = f.wait_job("no checksum").await;
        assert_eq!(j.phase, "failed");
        assert!(j.error.as_deref().unwrap().contains("no checksum"), "{j:?}");
        assert!(!f.root.join("bin/blvm").exists());
        f.clean();
    }

    #[tokio::test]
    async fn a_truncated_download_installs_nothing() {
        let f = fixture("cut").await;
        let body = script(VER, None);
        f.publish_with("v0.1.60", body.clone(), None, Some(body.len() / 2));
        f.begin(Kind::Install, None).await;
        let j = f.wait_job("truncated").await;
        assert_eq!(j.phase, "failed", "{j:?}");
        assert!(!f.root.join("bin/blvm").exists());
        assert!(!f.root.join("bin/staging").exists());
        f.clean();
    }

    #[tokio::test]
    async fn an_oversize_download_is_refused_before_it_starts() {
        let f = fixture("big").await;
        let body = script(VER, None);
        f.publish("v0.1.60", body);
        let p = f.published.lock().unwrap().clone();
        f.relist(&p, Some(("v0.1.60", MAX_DOWNLOAD + 1)), None);
        f.begin(Kind::Install, None).await;
        let j = f.wait_job("oversize").await;
        assert_eq!(j.phase, "failed");
        assert!(j.error.as_deref().unwrap().contains("more than"), "{j:?}");
        assert_eq!(f.fake.count("/dl/v0.1.60/blvm-"), 0, "nothing was fetched");
        f.clean();
    }

    #[tokio::test]
    async fn a_download_address_outside_the_release_downloads_is_not_trusted() {
        let f = fixture("host").await;
        f.publish("v0.1.60", script(VER, None));
        let p = f.published.lock().unwrap().clone();
        f.relist(&p, None, Some("http://127.0.0.1:1/evil"));
        f.begin(Kind::Install, None).await;
        let j = f.wait_job("untrusted").await;
        assert_eq!(j.phase, "failed");
        assert!(j.error.as_deref().unwrap().contains("does not trust"), "{j:?}");
        f.clean();
    }

    #[tokio::test]
    async fn a_redirect_to_another_host_is_refused() {
        let f = fixture("redir").await;
        let body = script(VER, None);
        f.publish("v0.1.60", body.clone());
        // `localhost` is not on the allowed list (only 127.0.0.1 is).
        let target = format!("http://localhost:{}/dl/v0.1.60/real", f.fake.addr.port());
        f.fake.set("/dl/v0.1.60/blvm-0.1.60-linux-x86_64", Resp { status: 302, location: Some(target), ..Default::default() });
        f.fake.set("/dl/v0.1.60/real", Resp::ok(body));
        f.begin(Kind::Install, None).await;
        let j = f.wait_job("redirect").await;
        assert_eq!(j.phase, "failed");
        assert_eq!(f.fake.count("/dl/v0.1.60/real"), 0, "the redirect was not followed");
        f.clean();
    }

    #[tokio::test]
    async fn a_program_that_will_not_run_is_stopped_at_the_test_and_the_old_one_stays() {
        let f = fixture("junk").await;
        f.publish("v0.1.59", script("0.1.59", None));
        f.begin(Kind::Install, Some("v0.1.59")).await;
        assert_eq!(f.wait_job("first").await.phase, "done");

        // Garbage that is not a program (a wrong CPU or a damaged file looks like this).
        let junk = b"\x7fELF-not-really, just bytes".to_vec();
        f.publish("v0.1.60", junk);
        let (code, _) = f.begin(Kind::Update, None).await;
        assert_eq!(code, 202);
        let j = f.wait_job("junk").await;
        assert_eq!(j.phase, "failed");
        let e = j.error.unwrap();
        assert!(e.contains("Unsupported platform") || e.contains("cannot run") || e.contains("does not run"), "{e}");
        assert!(!j.rolled_back, "nothing had been replaced yet");
        assert_eq!(f.link_target().as_deref(), Some("blvm-v0.1.59"), "the old program is still current");
        assert_eq!(releases::read_installed(&f.root).unwrap().tag, "v0.1.59");
        assert!(!f.root.join("bin/staging").exists());
        assert!(!f.root.join("bin/blvm-v0.1.60").exists());
        f.clean();
    }

    #[tokio::test]
    async fn update_restarts_a_running_node_on_the_new_program() {
        let f = fixture("update").await;
        f.publish("v0.1.59", script("0.1.59", None));
        f.begin(Kind::Install, Some("0.1.59")).await;
        assert_eq!(f.wait_job("first").await.phase, "done");

        let agent = f.agent();
        agent.start().await.unwrap();
        for _ in 0..500 {
            if agent.state() == ProcState::Running && agent.logs(None, 50).iter().any(|l| l.text.contains("node v0.1.59 up")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let old_pid = agent.view().pid.unwrap();

        f.publish("v0.1.60", script("0.1.60", None));
        let (code, v) = f.begin(Kind::Update, None).await;
        assert_eq!(code, 202, "{v}");
        let j = f.wait_job("update").await;
        assert_eq!(j.phase, "done", "{j:?}");
        assert_eq!(j.kind, Some("update"));
        assert_ne!(agent.view().pid, Some(old_pid), "a new process");
        assert!(!crate::agent::pid_alive(old_pid));
        for _ in 0..200 {
            if agent.logs(None, 100).iter().any(|l| l.text.contains("node v0.1.60 up")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(agent.logs(None, 100).iter().any(|l| l.text.contains("node v0.1.60 up")));
        let rec = releases::read_installed(&f.root).unwrap();
        assert_eq!((rec.tag.as_str(), rec.previous_tag.as_deref()), ("v0.1.60", Some("v0.1.59")));
        assert!(f.root.join("bin/blvm-v0.1.59").is_file(), "the previous version is kept");
        assert_eq!(f.link_target().as_deref(), Some("blvm-v0.1.60"));
        let m = f.snapshot().await.managed.unwrap();
        assert_eq!(m.node_version.as_deref(), Some("v0.1.60"));

        // A third version keeps only the last two on disk.
        f.publish("v0.1.61", script("0.1.61", None));
        f.begin(Kind::Update, None).await;
        assert_eq!(f.wait_job("third").await.phase, "done");
        assert!(!f.root.join("bin/blvm-v0.1.59").exists(), "older than the previous one: removed");
        assert!(f.root.join("bin/blvm-v0.1.60").is_file() && f.root.join("bin/blvm-v0.1.61").is_file());
        agent.stop_and_wait().await;
        f.clean();
    }

    #[tokio::test]
    async fn a_new_program_that_crashes_on_start_is_rolled_back() {
        let f = fixture("rollback").await;
        f.publish("v0.1.59", script("0.1.59", None));
        f.begin(Kind::Install, Some("0.1.59")).await;
        assert_eq!(f.wait_job("first").await.phase, "done");
        let agent = f.agent();
        agent.start().await.unwrap();
        for _ in 0..500 {
            if agent.state() == ProcState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The new program never answers its RPC and dies at once.
        f.ready.store(false, Ordering::Relaxed);
        f.publish("v0.1.60", script("0.1.60", Some("0")));
        f.begin(Kind::Update, None).await;
        let j = f.wait_job("rollback").await;
        assert_eq!(j.phase, "failed", "{j:?}");
        assert!(j.rolled_back, "{j:?}");
        let e = j.error.unwrap();
        assert!(e.contains("v0.1.60") && e.contains("previous program was put back"), "{e}");
        assert_eq!(f.link_target().as_deref(), Some("blvm-v0.1.59"));
        assert_eq!(releases::read_installed(&f.root).unwrap().tag, "v0.1.59");
        // The node is on the old program again.
        f.ready.store(true, Ordering::Relaxed);
        for _ in 0..500 {
            if agent.state() == ProcState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(agent.state(), ProcState::Running);
        assert!(agent.logs(None, 200).iter().filter(|l| l.text.contains("node v0.1.59 up")).count() >= 2);
        let m = f.snapshot().await.managed.unwrap();
        assert_eq!(m.node_version.as_deref(), Some("v0.1.59"));
        assert_eq!((m.update.phase, m.update.rolled_back), ("failed", true));
        agent.stop_and_wait().await;
        f.clean();
    }

    #[tokio::test]
    async fn a_crash_soon_after_the_node_first_answered_also_rolls_back() {
        let f = fixture("rollback-late").await;
        {
            let mut g = f.state.write().await;
            g.console.installer_timings.watch = Duration::from_secs(8);
        }
        f.publish("v0.1.59", script("0.1.59", None));
        f.begin(Kind::Install, Some("0.1.59")).await;
        assert_eq!(f.wait_job("first").await.phase, "done");
        let agent = f.agent();
        agent.start().await.unwrap();
        for _ in 0..500 {
            if agent.state() == ProcState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Answers the RPC (ready is true) and then dies after a second.
        f.publish("v0.1.60", script("0.1.60", Some("1")));
        f.begin(Kind::Update, None).await;
        let first = f.wait_job("done first").await;
        assert_eq!(first.phase, "done", "the node answered, so the job reports done at first");
        // ... and the job turns into a failure with the old program back, once the crash is seen.
        let mut j = first;
        for _ in 0..600 {
            j = f.cell().lock().unwrap().clone();
            if j.phase == "failed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(j.phase, "failed", "{j:?}");
        assert!(j.rolled_back);
        assert!(j.error.as_deref().unwrap().contains("crashed soon after"), "{j:?}");
        assert_eq!(f.link_target().as_deref(), Some("blvm-v0.1.59"));
        agent.request_stop();
        agent.wait_inactive(Duration::from_secs(10)).await;
        f.clean();
    }

    #[tokio::test]
    async fn refusals_are_plain_and_busy_is_409() {
        let f = fixture("refuse").await;
        f.publish("v0.1.60", script(VER, None));

        // Update with nothing installed; a bad tag; an unsupported platform.
        let (c, v) = f.begin(Kind::Update, None).await;
        assert_eq!(c, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("Install one first"));
        let (c, v) = f.begin(Kind::Install, Some("latest; rm -rf /")).await;
        assert_eq!(c, 400);
        assert!(v["error"].as_str().unwrap().contains("not a release number"));
        f.state.write().await.console.platform_override = Some("unsupported".into());
        let (c, v) = f.begin(Kind::Install, None).await;
        assert_eq!(c, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("no official node build"), "{v}");
        f.state.write().await.console.platform_override = Some("linux-x86_64".into());

        // Busy: a job is in the middle of its work.
        {
            let cell = f.cell();
            let mut j = cell.lock().unwrap();
            j.id = 999;
            j.phase = "verify";
        }
        let (c, v) = f.begin(Kind::Install, None).await;
        assert_eq!((c, v["error"].as_str(), v["ok"].clone()), (409, Some("busy"), json!(false)), "{v}");
        *f.cell().lock().unwrap() = Job::idle();

        // The first install is allowed even with updates off or on the store channel; later ones are not.
        for ch in ["off", "store"] {
            f.state.write().await.console.settings.updates = ch.into();
            let r = managed_releases(&f).await;
            assert_eq!(r["can_install"], true, "{ch}: first install allowed: {r}");
        }
        f.begin(Kind::Install, None).await;
        assert_eq!(f.wait_job("first install on a locked channel").await.phase, "done");
        for (ch, word) in [("off", "turned off"), ("store", "App Store")] {
            f.state.write().await.console.settings.updates = ch.into();
            let r = managed_releases(&f).await;
            assert_eq!(r["can_install"], false, "{ch}: {r}");
            assert!(r["reason"].as_str().unwrap().contains(word), "{r}");
            f.publish("v0.1.61", script("0.1.61", None));
            let (c, v) = f.begin(Kind::Update, None).await;
            assert_eq!(c, 400, "{ch}: {v}");
        }
        // The environment decides over the file.
        {
            let mut g = f.state.write().await;
            g.console.settings.updates = "self".into();
            g.console.env.updates = Some("off".into());
        }
        assert_eq!(managed_releases(&f).await["channel"], "off");
        f.clean();
    }

    async fn managed_releases(f: &Fx) -> Value {
        crate::managed::releases_json(&f.state, false).await
    }

    #[tokio::test]
    async fn the_releases_answer_has_the_shape_the_page_reads() {
        let f = fixture("shape").await;
        f.publish("v0.1.59", script("0.1.59", None));
        f.publish("v0.1.60", script(VER, None));
        let r = managed_releases(&f).await;
        assert_eq!(r["installed"], Value::Null);
        assert_eq!(r["latest"]["tag"], "v0.1.60");
        assert_eq!(r["latest"]["published_at"], 1_791_775_990u64);
        assert_eq!(r["latest"]["notes_url"], "https://github.com/BTCDecoded/blvm/releases/tag/v0.1.60");
        assert_eq!(r["asset"]["name"], "blvm-0.1.60-linux-x86_64");
        assert_eq!(r["asset"]["platform"], "linux-x86_64");
        assert_eq!(r["platform"], "linux-x86_64");
        assert_eq!((r["can_install"].clone(), r["reason"].clone(), r["available"].clone()), (json!(true), Value::Null, json!(false)));
        assert_eq!(r["channel"], "self");

        f.begin(Kind::Install, Some("v0.1.59")).await;
        assert_eq!(f.wait_job("install old").await.phase, "done");
        let r = managed_releases(&f).await;
        assert_eq!(r["installed"]["tag"], "v0.1.59");
        assert!(r["installed"]["installed_at"].as_u64().unwrap() > 0);
        assert_eq!((r["available"].clone(), r["current"].clone()), (json!(true), json!("v0.1.59")));
        // The status says so too.
        {
            let g = f.state.write().await;
            g.console.releases.set_dir(None);
        }
        let m = f.snapshot().await.managed.unwrap();
        assert_eq!((m.update.available, m.update.latest.as_deref()), (true, Some("v0.1.60")));
        f.clean();
    }

    #[tokio::test]
    async fn the_release_list_is_cached_ten_minutes_survives_a_restart_and_falls_back_when_offline() {
        let f = fixture("cache").await;
        f.publish("v0.1.60", script(VER, None));
        let cache = f.state.read().await.console.releases.clone();
        let dir = f.dir.join("cachedir");
        cache.set_dir(Some(dir.clone()));
        let a = cache.get(false).await;
        assert_eq!(a.releases.len(), 1);
        assert!(!a.stale && a.error.is_none() && a.checked_at.is_some());
        cache.get(false).await;
        cache.get(false).await;
        assert_eq!(f.fake.count("/api/releases"), 1, "fresh for ten minutes: one request");
        assert!(dir.join("releases.json").is_file());

        // "Check now" may bypass the cache, once a minute.
        cache.get(true).await;
        assert_eq!(f.fake.count("/api/releases"), 2);
        cache.get(true).await;
        assert_eq!(f.fake.count("/api/releases"), 2, "a second force within a minute is ignored");

        // A restart reads the saved answer.
        let fresh = Cache::new(cache.source().clone());
        fresh.set_dir(Some(dir));
        assert_eq!(fresh.peek_latest().as_deref(), Some("v0.1.60"));
        assert!(fresh.needs_check());

        // Offline (here: the API answers 403 as GitHub does when limiting): the old answer, marked stale.
        let other = testkit::start().await;
        other.set("/api/releases", Resp { status: 403, body: b"{}".to_vec(), ..Default::default() });
        let down = Cache::new(Source { api_url: other.url("/api/releases"), https_only: false, ..Default::default() });
        let g = down.get(false).await;
        assert!(g.releases.is_empty() && g.stale);
        assert!(g.error.as_deref().unwrap().contains("limiting"), "{:?}", g.error);
        let again = down.get(false).await;
        assert!(again.stale);
        assert_eq!(other.count("/api/releases"), 1, "after a failure it waits before asking again");
        // Nothing reachable at all.
        let gone = Cache::new(Source { api_url: "http://127.0.0.1:1/api".into(), https_only: false, ..Default::default() });
        let g = gone.get(false).await;
        assert!(g.releases.is_empty() && g.error.as_deref().unwrap().contains("Could not reach"), "{:?}", g.error);
        f.clean();
    }

    #[tokio::test]
    async fn the_routes_work_end_to_end() {
        let f = fixture("http").await;
        f.publish("v0.1.60", script(VER, None));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let st = Arc::clone(&f.state);
        tokio::spawn(async move {
            let _ = crate::http::serve_listener(l, st).await;
        });
        let post = |path: &'static str, body: &'static str| async move {
            let req = format!(
                "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nX-BLVM-UI: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            raw(addr, req).await
        };
        let get = |path: &'static str| async move { raw(addr, format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")).await };

        let (c, v) = get("/api/node/releases").await;
        assert_eq!(c, 200);
        assert_eq!(v["latest"]["tag"], "v0.1.60");
        let (c, v) = get("/api/node/install").await;
        assert_eq!((c, v["phase"].as_str(), v["tag"].clone()), (200, Some("idle"), Value::Null));
        // `{}`, an empty body, and both key spellings.
        let (c, v) = post("/api/node/install", "{\"version\":\"0.1.60\",\"tag\":\"v0.1.60\"}").await;
        assert_eq!(c, 202, "{v}");
        assert_eq!(v["ok"], true);
        assert_eq!(v["job"]["tag"], "v0.1.60");
        f.wait_job("http install").await;
        let (c, v) = get("/api/node/install").await;
        assert_eq!((c, v["phase"].as_str(), v["tag"].as_str(), v["rolled_back"].clone()), (200, Some("done"), Some("v0.1.60"), json!(false)));
        let (c, v) = get("/api/status").await;
        assert_eq!(c, 200);
        assert_eq!(v["bundle_available"], true);
        let (c, v) = post("/api/node/update", "{}").await;
        assert_eq!(c, 400, "up to date: {v}");
        assert!(v["error"].as_str().unwrap().contains("newest"));
        let (c, _) = post("/api/node/install", "not json").await;
        assert_eq!(c, 400);
        f.clean();
    }

    async fn raw(addr: std::net::SocketAddr, req: String) -> (u16, Value) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        let code = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = text.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
        (code, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    // ---- review fixes ----

    #[tokio::test]
    async fn an_unknown_release_number_is_a_400_not_a_failed_job() {
        let f = fixture("norelease").await;
        f.publish("v0.1.60", script(VER, None));
        let (code, v) = f.begin(Kind::Install, Some("v9.9.9")).await;
        assert_eq!(code, 400, "{v}");
        assert_eq!(v["code"], "no_such_release");
        assert!(v["error"].as_str().unwrap().contains("v9.9.9"));
        assert!(!f.cell().lock().unwrap().active(), "no job was started");
        assert_eq!(f.cell().lock().unwrap().phase, "idle");
        // A release that exists still goes through.
        let (code, _) = f.begin(Kind::Install, Some("v0.1.60")).await;
        assert_eq!(code, 202);
        f.wait_job("known tag").await;
        f.clean();
    }

    #[tokio::test]
    async fn an_older_release_is_refused_unless_a_downgrade_is_asked_for() {
        let f = fixture("downgrade").await;
        f.publish("v0.1.59", script("0.1.59", None));
        f.publish("v0.1.60", script(VER, None));
        let (code, _) = f.begin(Kind::Install, Some("v0.1.60")).await;
        assert_eq!(code, 202);
        assert_eq!(f.wait_job("newer first").await.phase, "done");
        let (code, v) = f.begin(Kind::Update, Some("v0.1.59")).await;
        assert_eq!((code, v["code"].as_str()), (400, Some("downgrade")), "{v}");
        assert!(v["error"].as_str().unwrap().contains("older"));
        assert_eq!(f.link_target().as_deref(), Some("blvm-v0.1.60"), "nothing changed");
        let (code, v) = f.begin_with(Kind::Update, Some("v0.1.59"), true).await;
        assert_eq!(code, 202, "{v}");
        assert_eq!(f.wait_job("downgrade").await.phase, "done");
        assert_eq!(f.link_target().as_deref(), Some("blvm-v0.1.59"));
        f.clean();
    }

    #[tokio::test]
    async fn a_download_that_trickles_past_the_overall_limit_is_given_up() {
        let f = fixture("trickle").await;
        let mut body = script(VER, None);
        body.extend(std::iter::repeat_n(b'#', 4000));
        f.publish("v0.1.60", body.clone());
        f.fake.set("/dl/v0.1.60/blvm-0.1.60-linux-x86_64", Resp { status: 200, body, trickle: Some((200, 60)), ..Default::default() });
        {
            let mut g = f.state.write().await;
            g.console.installer_timings.total = Duration::from_millis(400);
        }
        let (code, _) = f.begin(Kind::Install, None).await;
        assert_eq!(code, 202);
        let j = f.wait_job("trickle").await;
        assert_eq!(j.phase, "failed");
        assert!(j.error.as_deref().unwrap().contains("taking too long"), "{j:?}");
        assert!(!f.root.join("bin/blvm").exists() && !f.root.join("bin/staging").exists());
        f.clean();
    }

    #[tokio::test]
    async fn the_releases_answer_calls_nothing_custom_when_nothing_is_installed() {
        let f = fixture("custom").await;
        f.publish("v0.1.60", script(VER, None));
        let v = crate::managed::releases_json(&f.state, false).await;
        assert_eq!(v["custom"], false, "no program at all: nothing is custom");
        assert_eq!(v["current"], Value::Null);
        // A program somebody else put there is custom.
        let hand = f.dir.join("handmade.sh");
        std::fs::write(&hand, script("0.0.1", None)).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hand, std::fs::Permissions::from_mode(0o755)).unwrap();
        {
            let mut g = f.state.write().await;
            g.console.settings.node_bin = Some(hand.display().to_string());
            g.console.reresolve();
        }
        let v = crate::managed::releases_json(&f.state, false).await;
        assert_eq!(v["custom"], true);
        // Installed by the installer: not custom.
        let (code, _) = f.begin(Kind::Install, None).await;
        assert_eq!(code, 202);
        f.wait_job("install").await;
        let v = crate::managed::releases_json(&f.state, false).await;
        assert_eq!((v["custom"].clone(), v["current"].as_str()), (json!(false), Some("v0.1.60")));
        f.clean();
    }

    #[test]
    fn placing_a_program_flushes_the_file_and_the_folder_before_and_after_the_link_swap() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("blvm-installer-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let staged = root.join("bin/staging/v0.1.60/blvm-0.1.60-linux-x86_64");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, script(VER, None)).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before = crate::fsutil::sync_count();
        install_files(&root, "v0.1.60", "blvm-0.1.60-linux-x86_64", &"ab".repeat(32), &staged).unwrap();
        // bin/ after the rename, bin/ after the link swap, then installed.json (file + folder).
        assert!(crate::fsutil::sync_count() >= before + 4, "only {} flushes", crate::fsutil::sync_count() - before);
        assert!(root.join("bin/blvm").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_downloaded_file_is_flushed_to_the_disk_before_it_is_checked() {
        let f = fixture("syncdl").await;
        f.publish("v0.1.60", script(VER, None));
        let before = crate::fsutil::sync_count();
        let (code, _) = f.begin(Kind::Install, None).await;
        assert_eq!(code, 202);
        assert_eq!(f.wait_job("sync").await.phase, "done");
        // staged file, bin/ twice, installed.json twice: at least five on this thread.
        assert!(crate::fsutil::sync_count() >= before + 5, "only {} flushes", crate::fsutil::sync_count() - before);
        f.clean();
    }

    /// Collects what `tracing` prints (see `captured_log`).
    #[derive(Clone, Default)]
    struct LogBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> LogBuf {
            self.clone()
        }
    }

    /// What `tracing` printed in this test program since the first call (a process-wide subscriber, installed
    /// once: a subscriber that is only set for one thread misses log calls made on other threads, and the
    /// per-callsite interest cache is shared with every other test that runs at the same time).
    fn captured_log() -> String {
        static LOG: std::sync::OnceLock<LogBuf> = std::sync::OnceLock::new();
        let buf = LOG.get_or_init(|| {
            let buf = LogBuf::default();
            let sub = tracing_subscriber::fmt().with_writer(buf.clone()).with_ansi(false).with_max_level(tracing::Level::INFO).finish();
            let _ = tracing::subscriber::set_global_default(sub);
            buf
        });
        String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
    }

    #[tokio::test]
    async fn the_checked_hash_is_written_to_the_log() {
        let _ = captured_log(); // installed before anything under test logs
        let f = fixture("loghash").await;
        // A program of its own, so that no other test's line can have the same checksum.
        let mut body = script(VER, None);
        body.extend_from_slice(b"# the-checked-hash-is-written-to-the-log\n");
        f.publish("v0.1.60", body.clone());
        let (code, _) = f.begin(Kind::Install, None).await;
        assert_eq!(code, 202);
        assert_eq!(f.wait_job("log").await.phase, "done");
        let text = captured_log();
        let lines: Vec<&str> = text.lines().filter(|l| l.contains("checksum matches") && l.contains(&sha(&body))).collect();
        assert_eq!(lines.len(), 1, "{text}");
        assert!(lines[0].contains("INFO"), "{}", lines[0]);
        f.clean();
    }
}
