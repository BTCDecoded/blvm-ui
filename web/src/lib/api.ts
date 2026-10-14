import { friendlyErr } from "./format";
import {
  emptySnapshot,
  type ConsoleSettings,
  type LogsResponse,
  type ModuleAction,
  type ModulesResponse,
  type NetworkId,
  type NetworkPreview,
  type NetworkSwitchResponse,
  type NodeAction,
  type NodeActionResponse,
  type InstallJob,
  type JobPhase,
  type JobRequest,
  type JobResponse,
  type OkResponse,
  type ReleaseInfo,
  type SetupRequest,
  type SetupResponse,
  type SetupTestRequest,
  type SetupTestResponse,
  type Snapshot,
} from "./types";

/** Design F: mutating routes want JSON plus this header (blocks simple cross-site forms). */
const POST_HEADERS = { "content-type": "application/json", "x-blvm-ui": "1" };

async function jsonOrEmpty(r: Response): Promise<Record<string, unknown>> {
  return (await r.json().catch(() => ({}))) as Record<string, unknown>;
}

export async function fetchStatus(signal?: AbortSignal): Promise<Snapshot> {
  const r = await fetch("/api/status", { signal });
  if (!r.ok) {
    // A 403 carries the host's own explanation (open the console by an allowed name, BLVM_UI_ALLOWED_HOSTS).
    const j = (await r.json().catch(() => ({}))) as Record<string, unknown>;
    throw new ApiError(r.status, typeof j.error === "string" && j.error ? j.error : `status ${r.status}`);
  }
  return (await r.json()) as Snapshot;
}

export async function connectTo(addr: string): Promise<Snapshot> {
  const r = await fetch("/api/connect", {
    method: "POST",
    headers: POST_HEADERS,
    body: JSON.stringify({ rpc: addr }),
    signal: AbortSignal.timeout(4000),
  });
  if (!r.ok) throw new Error(`connect ${r.status}`);
  return (await r.json()) as Snapshot;
}

export async function toggleNode(): Promise<{
  ok: boolean;
  message: string;
  error?: string;
  status?: Snapshot;
}> {
  const r = await fetch("/api/node", {
    method: "POST",
    headers: POST_HEADERS,
    body: JSON.stringify({ action: "toggle" }),
  });
  return (await r.json()) as {
    ok: boolean;
    message: string;
    error?: string;
    status?: Snapshot;
  };
}

export async function callSetting(method: string, params: unknown[]): Promise<void> {
  const r = await fetch("/api/rpc", {
    method: "POST",
    headers: POST_HEADERS,
    body: JSON.stringify({ method, params }),
  });
  const j = await jsonOrEmpty(r);
  if (!r.ok || j.ok === false) {
    throw new Error(friendlyErr(j.error));
  }
}

// ---- Managed-node routes (design F). Every one of these is absent on a standalone / older host. ----

export class ApiError extends Error {
  status: number;
  /** The host's `needs` field on a refusal (`"wipe"`: the change needs the chain data deleted first). */
  needs?: string;
  constructor(status: number, message: string, needs?: string) {
    super(message);
    this.status = status;
    this.needs = needs;
  }
}

/** The host has no such route (an older or smaller host answers 404), as opposed to "it failed". */
export const isMissing = (e: unknown): boolean => e instanceof ApiError && e.status === 404;

async function call<T>(path: string, init: RequestInit & { timeout?: number } = {}): Promise<T> {
  const { timeout = 8000, ...rest } = init;
  const r = await fetch(path, { ...rest, signal: AbortSignal.timeout(timeout) });
  const j = (await r.json().catch(() => ({}))) as Record<string, unknown>;
  if (!r.ok) {
    const e = j.error;
    throw new ApiError(r.status, typeof e === "string" && e ? e : `${path} answered ${r.status}`, typeof j.needs === "string" ? j.needs : undefined);
  }
  return j as T;
}

/**
 * A GET whose answer must have a shape. A host that does not know the route may still answer 200 with the
 * page (old hosts did) or with `{}`: that is "missing" too, never a half-empty object the cards would crash on.
 */
async function getShaped<T>(path: string, ok: (j: Record<string, unknown>) => boolean): Promise<T> {
  const j = await call<Record<string, unknown>>(path);
  if (!ok(j)) throw new ApiError(404, "This console does not provide that.");
  return j as T;
}

const getJson = <T>(path: string) => call<T>(path);
const postJson = <T>(path: string, body: unknown, timeout?: number) =>
  call<T>(path, { method: "POST", headers: POST_HEADERS, body: JSON.stringify(body), timeout });

/**
 * POST /api/node with a managed action. The host answers at once ("Stopping the node...", "Starting the node...");
 * the node itself takes minutes to drain or to answer, and the page follows `managed.process.state` in the
 * status it already polls. So this waits for the answer only, not for the node.
 */
export const nodeAction = (action: NodeAction) => postJson<NodeActionResponse>("/api/node", { action });

export const previewSwitch = (to: NetworkId) =>
  getShaped<NetworkPreview>(`/api/network/preview?to=${encodeURIComponent(to)}`, (j) => typeof j.to === "string");

export const switchNetwork = (to: NetworkId) => postJson<NetworkSwitchResponse>("/api/network/switch", { to });

export const fetchLogs = (after?: number, lines = 200) =>
  getShaped<LogsResponse>(`/api/node/logs?lines=${lines}${after !== undefined ? `&after=${after}` : ""}`, (j) =>
    Array.isArray(j.lines),
  );

// ---- Node install / update (design D; `/api/node/install` has the same job shape as update) ----

const LEGACY_PHASE: Record<string, JobPhase> = {
  downloading: "download",
  verifying: "verify",
  installing: "install",
  restarting: "restart",
  rolled_back: "failed",
};
const PHASES: JobPhase[] = ["idle", "download", "verify", "test", "install", "restart", "done", "failed"];

/** A job as the host sent it (or `managed.update`) with every field defaulted; old phase names mapped. */
export function normalizeJob(j: Record<string, unknown> | null | undefined): InstallJob {
  const raw = String(j?.phase ?? "idle");
  const phase: JobPhase = LEGACY_PHASE[raw] ?? (PHASES.includes(raw as JobPhase) ? (raw as JobPhase) : "idle");
  const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) && v > 0 ? v : 0);
  return {
    phase,
    tag: typeof j?.tag === "string" && j.tag ? j.tag : null,
    bytes: num(j?.bytes),
    total: num(j?.total),
    error: typeof j?.error === "string" && j.error ? j.error : null,
    rolled_back: j?.rolled_back === true || raw === "rolled_back",
  };
}

export const jobRunning = (j: { phase: JobPhase }): boolean =>
  j.phase === "download" || j.phase === "verify" || j.phase === "test" || j.phase === "install" || j.phase === "restart";

/** `ReleaseInfo` with every field defaulted, so the cards never meet a missing key. */
export function normalizeReleases(j: Record<string, unknown>): ReleaseInfo {
  const obj = (v: unknown) => (v && typeof v === "object" ? (v as Record<string, unknown>) : null);
  const str = (v: unknown) => (typeof v === "string" && v ? v : null);
  const nn = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : null);
  const inst = obj(j.installed);
  const lat = obj(j.latest);
  const ass = obj(j.asset);
  const ch = j.channel === "self" || j.channel === "store" || j.channel === "off" ? j.channel : undefined;
  return {
    installed: inst && str(inst.tag) ? { tag: str(inst.tag)!, installed_at: nn(inst.installed_at) } : null,
    latest: lat && str(lat.tag) ? { tag: str(lat.tag)!, published_at: nn(lat.published_at), notes_url: str(lat.notes_url) } : null,
    asset: ass && str(ass.name) ? { name: str(ass.name)!, size: nn(ass.size) ?? 0, platform: str(ass.platform) ?? "" } : null,
    platform: str(j.platform) ?? "unsupported",
    can_install: j.can_install === true,
    reason: str(j.reason),
    available: j.available === true,
    offline: j.offline === true || (typeof j.unavailable_reason === "string" && j.unavailable_reason !== ""),
    ...(ch ? { channel: ch } : {}),
    // Absent on a host from before the page-size check: it then runs everywhere, as before.
    runs_here: j.runs_here !== false,
    runs_here_reason: str(j.runs_here_reason),
    page_size: nn(j.page_size),
  };
}

/** GET /api/node/releases. A host without the route (404, or 200 with a page / `{}`) is "missing" (isMissing). */
export const getReleases = async (force = false): Promise<ReleaseInfo> =>
  normalizeReleases(
    await getShaped<Record<string, unknown>>(
      // `force=1` skips the host's cache (it keeps a failed check for a minute): used by "Try again".
      force ? "/api/node/releases?force=1" : "/api/node/releases",
      (j) => typeof j.can_install === "boolean" || typeof j.platform === "string",
    ),
  );

/** GET /api/node/install: the current (or last) install/update job. 404 = this host cannot install. */
export const getInstallJob = async (): Promise<InstallJob> =>
  normalizeJob(await getShaped<Record<string, unknown>>("/api/node/install", (j) => typeof j.phase === "string"));

const jobBody = (tag?: string, force = false): JobRequest => ({
  ...(tag ? { tag, version: tag.replace(/^v/, "") } : {}),
  ...(force ? { force: true } : {}),
});
/** POST /api/node/install `{tag?, force?}`: first install (or replacing a custom build). 202 + job, or `{ok:false,error}`. */
export const installNode = (tag?: string, force = false) => postJson<JobResponse>("/api/node/install", jobBody(tag, force));
/** POST /api/node/update `{tag?, force?}`: replace the installed release. Same answer as install. */
export const updateNode = (tag?: string, force = false) => postJson<JobResponse>("/api/node/update", jobBody(tag, force));

export const fetchModules = () =>
  getShaped<ModulesResponse>("/api/modules", (j) => Array.isArray(j.index) && Array.isArray(j.installed));

export const moduleAction = (action: ModuleAction, name: string) =>
  postJson<OkResponse>(`/api/modules/${action}`, { name });

export const getSettings = () => getJson<ConsoleSettings>("/api/settings");

/** Deleting chain data (`confirm_wipe`) can take minutes on a big store on a slow disk: the host finishes it
 *  whatever the page does, but the page should wait for the answer rather than give up after 8 s. */
const WIPE_TIMEOUT_MS = 10 * 60 * 1000;
const wipes = (body: unknown): boolean =>
  typeof body === "object" && body !== null && (body as { confirm_wipe?: unknown }).confirm_wipe === true;

export const patchSettings = (patch: ConsoleSettings) =>
  postJson<ConsoleSettings>("/api/settings", patch, wipes(patch) ? WIPE_TIMEOUT_MS : undefined);

export const runSetup = (req: SetupRequest) => postJson<SetupResponse>("/api/setup", req, wipes(req) ? WIPE_TIMEOUT_MS : 20000);

export const testConnection = (req: SetupTestRequest) => postJson<SetupTestResponse>("/api/setup/test", req, 10000);

export { emptySnapshot };
