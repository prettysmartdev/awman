// Storage policy (design §5.7). The ONLY browser storage the app uses:
// sessionStorage (per tab) holding
//   awman.web.resume = {instanceId, frontendId, tabId, launchContext}
//   awman.web.view   = {panelSizes, focus}
// Never tokens, grants, polling secrets, snapshots, command text or terminal
// bytes. localStorage / IndexedDB / cookies are never used (ESLint-enforced;
// the built-output audit also fails on them).

export const RESUME_KEY = 'awman.web.resume';
export const VIEW_KEY = 'awman.web.view';

export interface ResumeRecord {
  instanceId: string;
  frontendId: string;
  tabId: string | null;
  launchContext: string | null;
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

export function readResume(storage: StorageLike | null = defaultStorage()): ResumeRecord | null {
  const value = read(storage, RESUME_KEY) as Partial<ResumeRecord> | null;
  if (!value || !isId(value.instanceId) || !isId(value.frontendId)) return null;
  return {
    instanceId: value.instanceId,
    frontendId: value.frontendId,
    tabId: isId(value.tabId) ? value.tabId : null,
    launchContext: isId(value.launchContext) ? value.launchContext : null,
  };
}

export function writeResume(record: ResumeRecord, storage: StorageLike | null = defaultStorage()): void {
  if (!isId(record.instanceId) || !isId(record.frontendId)) return;
  write(storage, RESUME_KEY, {
    instanceId: record.instanceId,
    frontendId: record.frontendId,
    tabId: isId(record.tabId) ? record.tabId : null,
    launchContext: isId(record.launchContext) ? record.launchContext : null,
  });
}

export function clearResume(storage: StorageLike | null = defaultStorage()): void {
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
