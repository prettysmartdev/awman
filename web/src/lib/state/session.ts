// Storage policy (design §5.7; WI 0160 C4). sessionStorage (per tab) holds
//   awman.web.resume = {instanceId, frontendId, tabId, launchContext,
//                       version:2, credentialId, daemonEpoch, listenerEpoch}
//   awman.web.view   = {panelSizes, focus}
// Never tokens, grants, polling secrets, proofs, snapshots, command text or
// terminal bytes. The only other storage is the saved signing key in
// IndexedDB (src/lib/client/credentialStore.ts); localStorage and cookies are
// never used (ESLint-enforced; the built-output audit also fails on them).
//
// A resume hint is bound to the saved authorization it was written under. It
// is used only after that credential recovered the grant in this document
// (setResumeBinding), so a legacy hint, a hint from another daemon/listener
// generation or one from a re-linked browser never auto-resumes.

export const RESUME_KEY = 'awman.web.resume';
export const VIEW_KEY = 'awman.web.view';

export interface ResumeRecord {
  instanceId: string;
  frontendId: string;
  tabId: string | null;
  launchContext: string | null;
}

/** The saved authorization a resume hint belongs to (C4 version-2 fields). */
export interface ResumeBinding {
  credentialId: string;
  daemonEpoch: string;
  listenerEpoch: string;
}

export interface ResumeHint {
  record: ResumeRecord;
  binding: ResumeBinding;
}

export interface ViewPrefs {
  panelSizes: Record<string, number>;
  focus: string | null;
}

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

// Daemon ids look like `ins_…`, `frn_…`, `tab_…`, `lc_…`. Credentials
// (`awb_` grants, `awt_` tickets) are refused outright.
const ID_RE = /^[a-z]{2,8}_[A-Za-z0-9-]{1,64}$/;
const CREDENTIAL_RE = /^(?:awb|awt|blk)_/;

function isId(value: unknown): value is string {
  return typeof value === 'string' && ID_RE.test(value) && !CREDENTIAL_RE.test(value);
}

const CREDENTIAL_ID_RE = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/; // 32 bytes, canonical unpadded base64url
const EPOCH_RE = /^[A-Za-z0-9_-]{21}[AQgw]$/; // 16 bytes, canonical unpadded base64url

function isBinding(value: Partial<ResumeBinding>): value is ResumeBinding {
  return typeof value.credentialId === 'string' && CREDENTIAL_ID_RE.test(value.credentialId) && !CREDENTIAL_RE.test(value.credentialId) && typeof value.daemonEpoch === 'string' && EPOCH_RE.test(value.daemonEpoch) && typeof value.listenerEpoch === 'string' && EPOCH_RE.test(value.listenerEpoch);
}

let activeBinding: ResumeBinding | null = null;
/**
 * The unbound hint this same document wrote under a memory-only grant. Only
 * it may be read back (same grant, same document); after a reload the value
 * is gone, so a legacy hint never auto-resumes.
 */
let documentLegacyHint: string | null = null;

/**
 * The saved authorization this document recovered or enrolled (null for a
 * memory-only grant or after authorization ended). Hints are read and
 * written only under it.
 */
export function setResumeBinding(binding: ResumeBinding | null): void {
  documentLegacyHint = null;
  activeBinding = binding && isBinding(binding) ? { credentialId: binding.credentialId, daemonEpoch: binding.daemonEpoch, listenerEpoch: binding.listenerEpoch } : null;
}

export function resumeBinding(): ResumeBinding | null {
  return activeBinding;
}

function sameBinding(a: ResumeBinding, b: ResumeBinding): boolean {
  return a.credentialId === b.credentialId && a.daemonEpoch === b.daemonEpoch && a.listenerEpoch === b.listenerEpoch;
}

function defaultStorage(): StorageLike | null {
  try {
    return typeof window === 'undefined' ? null : window.sessionStorage;
  } catch {
    return null; // storage disabled (privacy mode, sandbox)
  }
}

function read(storage: StorageLike | null, key: string): unknown {
  if (!storage) return null;
  try {
    const raw = storage.getItem(key);
    if (!raw || raw.length > 4096) return null;
    return JSON.parse(raw) as unknown;
  } catch {
    return null;
  }
}

function write(storage: StorageLike | null, key: string, value: unknown): void {
  if (!storage) return;
  try {
    storage.setItem(key, JSON.stringify(value));
  } catch {
    // quota / disabled: resume is a convenience only
  }
}

/**
 * This tab's version-2 hint and its binding, validated but NOT yet matched
 * to a recovered credential. Used only to choose which saved authorization
 * to prove; legacy (unbound) hints return null.
 */
export function readResumeHint(storage: StorageLike | null = defaultStorage()): ResumeHint | null {
  const value = read(storage, RESUME_KEY) as (Partial<ResumeRecord> & Partial<ResumeBinding> & { version?: unknown }) | null;
  if (!value || value.version !== 2 || !isId(value.instanceId) || !isId(value.frontendId)) return null;
  const binding: Partial<ResumeBinding> = { credentialId: value.credentialId, daemonEpoch: value.daemonEpoch, listenerEpoch: value.listenerEpoch };
  if (!isBinding(binding)) return null;
  return {
    record: {
      instanceId: value.instanceId,
      frontendId: value.frontendId,
      tabId: isId(value.tabId) ? value.tabId : null,
      launchContext: isId(value.launchContext) ? value.launchContext : null,
    },
    binding,
  };
}

/**
 * The resume ids, only when bound to the authorization this document holds
 * (or an unbound hint this same document wrote under its memory-only grant).
 */
export function readResume(storage: StorageLike | null = defaultStorage()): ResumeRecord | null {
  const hint = readResumeHint(storage);
  if (hint) return activeBinding && sameBinding(hint.binding, activeBinding) ? hint.record : null;
  if (activeBinding || documentLegacyHint === null) return null;
  let raw: string | null;
  try {
    raw = storage?.getItem(RESUME_KEY) ?? null;
  } catch {
    return null;
  }
  if (raw !== documentLegacyHint) return null;
  const value = read(storage, RESUME_KEY) as Partial<ResumeRecord> | null;
  if (!value || !isId(value.instanceId) || !isId(value.frontendId)) return null;
  return { instanceId: value.instanceId, frontendId: value.frontendId, tabId: isId(value.tabId) ? value.tabId : null, launchContext: isId(value.launchContext) ? value.launchContext : null };
}

/** Write the hint under the active binding (version 2); a memory-only grant writes an unbound, same-document hint. */
export function writeResume(record: ResumeRecord, storage: StorageLike | null = defaultStorage()): void {
  if (!isId(record.instanceId) || !isId(record.frontendId)) return;
  if (!activeBinding) {
    const legacy = { instanceId: record.instanceId, frontendId: record.frontendId, tabId: isId(record.tabId) ? record.tabId : null, launchContext: isId(record.launchContext) ? record.launchContext : null };
    write(storage, RESUME_KEY, legacy);
    documentLegacyHint = JSON.stringify(legacy);
    return;
  }
  documentLegacyHint = null;
  write(storage, RESUME_KEY, {
    instanceId: record.instanceId,
    frontendId: record.frontendId,
    tabId: isId(record.tabId) ? record.tabId : null,
    launchContext: isId(record.launchContext) ? record.launchContext : null,
    version: 2,
    credentialId: activeBinding.credentialId,
    daemonEpoch: activeBinding.daemonEpoch,
    listenerEpoch: activeBinding.listenerEpoch,
  });
}

export function clearResume(storage: StorageLike | null = defaultStorage()): void {
  documentLegacyHint = null;
  try {
    storage?.removeItem(RESUME_KEY);
  } catch {
    // ignore
  }
}

const PREF_KEY_RE = /^[a-z][a-zA-Z0-9_.-]{0,40}$/;

/** Keep only well-formed view prefs (bounded, numeric sizes, simple keys). */
export function sanitizeViewPrefs(value: unknown): ViewPrefs {
  const prefs = (value ?? {}) as Partial<ViewPrefs>;
  const panelSizes: Record<string, number> = {};
  if (typeof prefs.panelSizes === 'object' && prefs.panelSizes !== null) {
    for (const [key, size] of Object.entries(prefs.panelSizes).slice(0, 32)) {
      if (PREF_KEY_RE.test(key) && typeof size === 'number' && Number.isFinite(size) && size >= 0 && size <= 10_000) panelSizes[key] = size;
    }
  }
  const focus = typeof prefs.focus === 'string' && PREF_KEY_RE.test(prefs.focus) ? prefs.focus : null;
  return { panelSizes, focus };
}

export function readViewPrefs(storage: StorageLike | null = defaultStorage()): ViewPrefs {
  return sanitizeViewPrefs(read(storage, VIEW_KEY));
}

export function writeViewPrefs(prefs: ViewPrefs, storage: StorageLike | null = defaultStorage()): void {
  write(storage, VIEW_KEY, sanitizeViewPrefs(prefs));
}
