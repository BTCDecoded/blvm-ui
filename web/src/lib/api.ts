import { friendlyErr } from "./format";
import { emptySnapshot, type Snapshot } from "./types";

async function jsonOrEmpty(r: Response): Promise<Record<string, unknown>> {
  return (await r.json().catch(() => ({}))) as Record<string, unknown>;
}

export async function fetchStatus(signal?: AbortSignal): Promise<Snapshot> {
  const r = await fetch("/api/status", { signal });
  if (!r.ok) throw new Error(`status ${r.status}`);
  return (await r.json()) as Snapshot;
}

export async function connectTo(addr: string): Promise<Snapshot> {
  const r = await fetch("/api/connect", {
    method: "POST",
    headers: { "content-type": "application/json" },
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
    headers: { "content-type": "application/json" },
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
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ method, params }),
  });
  const j = await jsonOrEmpty(r);
  if (!r.ok || j.ok === false) {
    throw new Error(friendlyErr(j.error));
  }
}

export { emptySnapshot };
