import type { ModuleIndexEntry, ModulesResponse } from "./types";

/** Display name for a module: the index title, else the module name without its "blvm-" prefix. */
export function moduleTitle(name: string, e?: ModuleIndexEntry): string {
  if (e?.title) return e.title;
  const bare = name.replace(/^blvm-/, "");
  return bare.charAt(0).toUpperCase() + bare.slice(1);
}

/**
 * Why an index entry cannot be installed right now, or null when it can (design E, K step 5):
 * no release yet, no build for this platform, or the host cannot install at all.
 */
export function moduleReason(e: ModuleIndexEntry, data: ModulesResponse | null): string | null {
  if (e.coming_soon) return "Coming soon. There is no release yet.";
  if (data && !data.can_install) return data.reason || "Installing is not available on this setup.";
  if (data?.platform && e.platforms && e.platforms.length > 0 && !e.platforms.includes(data.platform)) {
    return `No build for this computer (${data.platform}).`;
  }
  return null;
}

/** Page ids (design L) → how the sidebar shows them and what installs them. */
export const MODULE_PAGES: Record<string, { label: string; icon: string; installHint: string }> = {
  mining: { label: "Mining", icon: "mining", installHint: "Stratum V2 or DATUM" },
};
