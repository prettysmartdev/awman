// The ONE browser-credential persistence module (WI 0160 C1, C4).
//
// IndexedDB database `awman-browser-auth` v1, store `credentials`, key path
// `credentialId`. Each record holds a non-extractable, sign-only ECDSA P-256
// private CryptoKey plus public identifiers; never a bearer grant, polling
// secret, proof, signature, command, snapshot or terminal byte. Private key
// bytes are never exported, wrapped or serialized: the browser stores the
// CryptoKey object by structured clone.
//
// IndexedDB content is untrusted (same-origin script can rewrite it): every
// read validates the exact schema and the key's properties and drops
// failures. A stored daemonBase is a comparison value, never a destination.
import { decodeBase64Url, encodeBase64Url, isBase64UrlOf, randomBase64Url } from './base64url';
import { DAEMON_HTTP } from './http';

export const CREDENTIAL_DB = 'awman-browser-auth';
export const CREDENTIAL_DB_VERSION = 1;
export const CREDENTIAL_STORE = 'credentials';
export const MAX_CREDENTIALS = 8;
const IDB_TIMEOUT_MS = 5_000;

export interface CredentialRecord {
  version: 1;
  credentialId: string;
  privateKey: CryptoKey;
  publicKey: string;
  origin: string;
  daemonBase: string;
  listener: string;
  daemonEpoch: string;
  listenerEpoch: string;
  grantId: string | null;
  expiresAt: string | null;
  linkExpiresAt: string | null;
}

/** Public metadata that may change after link poll or proof. */
export type CredentialPatch = Partial<Pick<CredentialRecord, 'grantId' | 'expiresAt' | 'linkExpiresAt'>>;

export interface ListenerBinding {
  listener: string;
  daemonEpoch: string;
  listenerEpoch: string;
}

/** Thin storage seam: IndexedDB in browsers, an in-memory map in unit tests. */
export interface CredentialBackend {
  getAll(): Promise<unknown[]>;
  get(credentialId: string): Promise<unknown>;
  put(record: CredentialRecord): Promise<void>;
  delete(credentialId: string): Promise<void>;
}

export class CredentialStoreFull extends Error {
  override readonly name = 'CredentialStoreFull';
  constructor() {
    super('This browser already holds the maximum number of saved authorizations.');
  }
}

const RECORD_KEYS = ['version', 'credentialId', 'privateKey', 'publicKey', 'origin', 'daemonBase', 'listener', 'daemonEpoch', 'listenerEpoch', 'grantId', 'expiresAt', 'linkExpiresAt'].sort().join(',');
export const LISTENER_NAME_RE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
const GRANT_ID_RE = /^brw_[A-Za-z0-9_-]{1,64}$/;
const TIMESTAMP_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-]\d{2}:\d{2})$/;

function isOrigin(value: unknown): value is string {
  if (typeof value !== 'string' || value.length > 256) return false;
  try {
    return new URL(value).origin === value;
  } catch {
    return false;
  }
}

function isTimestampOrNull(value: unknown): value is string | null {
  return value === null || (typeof value === 'string' && value.length <= 40 && TIMESTAMP_RE.test(value) && Number.isFinite(Date.parse(value)));
}

export function isEpoch(value: unknown): value is string {
  return isBase64UrlOf(value, 16);
}

export function isCredentialId(value: unknown): value is string {
  return isBase64UrlOf(value, 32);
}

function isPublicKey(value: unknown): value is string {
  if (!isBase64UrlOf(value, 65)) return false;
  return decodeBase64Url(value)?.[0] === 0x04;
}

/** A private, non-extractable, sign-only ECDSA P-256 CryptoKey. */
export function isSigningKey(value: unknown): value is CryptoKey {
  if (typeof CryptoKey === 'undefined' || !(value instanceof CryptoKey)) return false;
  const algorithm = value.algorithm as Partial<EcKeyAlgorithm>;
  return (
    value.type === 'private' &&
    value.extractable === false &&
    algorithm.name === 'ECDSA' &&
    algorithm.namedCurve === 'P-256' &&
    value.usages.length === 1 &&
    value.usages[0] === 'sign'
  );
}

/** Exact C4 schema check; anything else is dropped. */
export function validateRecord(value: unknown): CredentialRecord | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const record = value as Record<string, unknown>;
  if (Object.keys(record).sort().join(',') !== RECORD_KEYS) return null;
  if (record.version !== 1) return null;
  if (!isCredentialId(record.credentialId) || !isPublicKey(record.publicKey)) return null;
  if (!isOrigin(record.origin) || !isOrigin(record.daemonBase)) return null;
  if (typeof record.listener !== 'string' || !LISTENER_NAME_RE.test(record.listener)) return null;
  if (!isEpoch(record.daemonEpoch) || !isEpoch(record.listenerEpoch)) return null;
  if (!(record.grantId === null || (typeof record.grantId === 'string' && GRANT_ID_RE.test(record.grantId)))) return null;
  if (!isTimestampOrNull(record.expiresAt) || !isTimestampOrNull(record.linkExpiresAt)) return null;
  if (!isSigningKey(record.privateKey)) return null;
  return record as unknown as CredentialRecord;
}

function promised<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error('IndexedDB request failed'));
  });
}

function withTimeout<T>(promise: Promise<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('IndexedDB did not answer')), IDB_TIMEOUT_MS);
    promise.then(
      value => {
        clearTimeout(timer);
        resolve(value);
      },
      error => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

// The only IndexedDB access in the app (audited by scripts/audit-output.mjs,
// which admits exactly this open call and no other storage API use).
function openRequest(): IDBOpenDBRequest | null {
  try {
    return indexedDB.open('awman-browser-auth', 1);
  } catch {
    return null; // ReferenceError (no IndexedDB) or SecurityError (storage disabled)
  }
}

/** IndexedDB-backed store, or null when this context has no usable IndexedDB. */
export function indexedDbBackend(): CredentialBackend | null {
  if (typeof globalThis.isSecureContext === 'boolean' && !globalThis.isSecureContext) return null;
  let database: Promise<IDBDatabase> | null = null;
  const open = (): Promise<IDBDatabase> => {
    database ??= withTimeout(
      new Promise<IDBDatabase>((resolve, reject) => {
        const request = openRequest();
        if (!request) {
          reject(new Error('IndexedDB unavailable'));
          return;
        }
        request.onupgradeneeded = () => {
          if (!request.result.objectStoreNames.contains(CREDENTIAL_STORE)) request.result.createObjectStore(CREDENTIAL_STORE, { keyPath: 'credentialId' });
        };
        request.onsuccess = () => {
          const db = request.result;
          db.onversionchange = () => db.close();
          resolve(db);
        };
        request.onerror = () => reject(request.error ?? new Error('IndexedDB open failed'));
        request.onblocked = () => reject(new Error('IndexedDB open blocked'));
      }),
    ).catch(error => {
      database = null;
      throw error;
    });
    return database;
  };
  const run = async <T>(mode: IDBTransactionMode, body: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> => {
    const db = await open();
    const transaction = db.transaction(CREDENTIAL_STORE, mode);
    const result = promised(body(transaction.objectStore(CREDENTIAL_STORE)));
    const done = new Promise<void>((resolve, reject) => {
      transaction.oncomplete = () => resolve();
      transaction.onerror = () => reject(transaction.error ?? new Error('IndexedDB transaction failed'));
      transaction.onabort = () => reject(transaction.error ?? new Error('IndexedDB transaction aborted'));
    });
    const [value] = await withTimeout(Promise.all([result, done]));
    return value;
  };
  return {
    getAll: () => run('readonly', store => store.getAll() as IDBRequest<unknown[]>),
    get: id => run('readonly', store => store.get(id) as IDBRequest<unknown>),
    put: async record => {
      await run('readwrite', store => store.put(record));
    },
    delete: async id => {
      await run('readwrite', store => store.delete(id));
    },
  };
}

/** In-memory backend for tests and for contexts that must not persist. */
export function memoryBackend(): CredentialBackend & { data: Map<string, unknown> } {
  const data = new Map<string, unknown>();
  return {
    data,
    getAll: async () => [...data.values()],
    get: async id => data.get(id),
    put: async record => void data.set(record.credentialId, structuredClone(record)),
    delete: async id => void data.delete(id),
  };
}

export interface StoreConfig {
  /** Trusted page origin (`location.origin`). */
  origin: string;
  /** Trusted daemon base (`DAEMON_HTTP`); stored values are compared with it, never fetched. */
  daemonBase: string;
}

export interface LoadedCredentials {
  /** False when no IndexedDB is usable in this context. */
  available: boolean;
  /** Valid records for this origin and daemon base. */
  usable: CredentialRecord[];
  /** A valid record exists that names a different page origin. */
  foreignOrigin: boolean;
}

export interface NewCandidate {
  record: CredentialRecord;
  /** Raw uncompressed SEC1 public key, base64url (for link enrollment). */
  publicKey: string;
}

function defaultConfig(): StoreConfig {
  return { origin: typeof location !== 'undefined' && typeof location.origin === 'string' ? location.origin : '', daemonBase: DAEMON_HTTP };
}

async function signsWithPublicKey(privateKey: CryptoKey, publicKey: string): Promise<boolean> {
  const raw = decodeBase64Url(publicKey);
  if (!raw) return false;
  const verifier = await crypto.subtle.importKey('raw', raw, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
  const message = crypto.getRandomValues(new Uint8Array(32));
  const signature = await crypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, privateKey, message);
  return crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, verifier, signature, message);
}

export class CredentialStore {
  private readonly config: StoreConfig;

  constructor(
    private readonly backend: CredentialBackend | null = indexedDbBackend(),
    config?: StoreConfig,
  ) {
    this.config = config ?? defaultConfig();
  }

  get available(): boolean {
    return this.backend !== null;
  }

  /** Read, validate and classify every record; invalid records are deleted. */
  async load(): Promise<LoadedCredentials> {
    if (!this.backend) return { available: false, usable: [], foreignOrigin: false };
    let raw: unknown[];
    try {
      raw = await this.backend.getAll();
    } catch {
      return { available: false, usable: [], foreignOrigin: false };
    }
    const usable: CredentialRecord[] = [];
    let foreignOrigin = false;
    for (const value of raw) {
      const record = validateRecord(value);
      if (!record) {
        const id = (value as { credentialId?: unknown } | null)?.credentialId;
        if (typeof id === 'string' && id.length <= 128) await this.remove(id);
        continue;
      }
      if (record.origin === this.config.origin && record.daemonBase === this.config.daemonBase) usable.push(record);
      else if (record.origin !== this.config.origin) foreignOrigin = true;
    }
    return { available: true, usable, foreignOrigin };
  }

  /**
   * Fresh key pair and credential ID for one explicit link attempt (never a
   * stored key). The record is committed and read back, and the read-back key
   * must sign a message the public key verifies, before linking may use it.
   * Returns null when the browser cannot persist the key (memory-only).
   * Throws CredentialStoreFull at MAX_CREDENTIALS usable records.
   */
  async createCandidate(binding: ListenerBinding): Promise<NewCandidate | null> {
    if (!this.backend) return null;
    const loaded = await this.load();
    if (!loaded.available) return null;
    if (loaded.usable.length >= MAX_CREDENTIALS) throw new CredentialStoreFull();
    let credentialId: string | null = null;
    try {
      const pair = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify']);
      const publicKey = encodeBase64Url(new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey)));
      credentialId = randomBase64Url(32);
      const record: CredentialRecord = {
        version: 1,
        credentialId,
        privateKey: pair.privateKey,
        publicKey,
        origin: this.config.origin,
        daemonBase: this.config.daemonBase,
        listener: binding.listener,
        daemonEpoch: binding.daemonEpoch,
        listenerEpoch: binding.listenerEpoch,
        grantId: null,
        expiresAt: null,
        linkExpiresAt: null,
      };
      if (!validateRecord(record)) throw new Error('generated key does not meet the credential policy');
      await this.backend.put(record);
      const stored = validateRecord(await this.backend.get(credentialId));
      if (!stored || stored.publicKey !== publicKey || !(await signsWithPublicKey(stored.privateKey, publicKey))) throw new Error('key round trip failed');
      return { record: stored, publicKey };
    } catch {
      if (credentialId) await this.remove(credentialId);
      return null;
    }
  }

  /** Atomically replace the enumerated public metadata of one record. */
  async update(credentialId: string, patch: CredentialPatch): Promise<CredentialRecord | null> {
    if (!this.backend) return null;
    try {
      const current = validateRecord(await this.backend.get(credentialId));
      if (!current) return null;
      const next: CredentialRecord = {
        ...current,
        ...('grantId' in patch ? { grantId: patch.grantId ?? null } : {}),
        ...('expiresAt' in patch ? { expiresAt: patch.expiresAt ?? null } : {}),
        ...('linkExpiresAt' in patch ? { linkExpiresAt: patch.linkExpiresAt ?? null } : {}),
      };
      if (!validateRecord(next)) return null;
      await this.backend.put(next);
      return next;
    } catch {
      return null;
    }
  }

  /** Delete one record. A failed deletion cannot restore server authority. */
  async remove(credentialId: string): Promise<void> {
    try {
      await this.backend?.delete(credentialId);
    } catch {
      // The record is revalidated (and used only with a valid proof) on later load.
    }
  }

  /** Remove records bound to another daemon or listener generation. */
  async purgeOtherGenerations(binding: ListenerBinding): Promise<CredentialRecord[]> {
    const { usable } = await this.load();
    const kept: CredentialRecord[] = [];
    for (const record of usable) {
      if (record.daemonEpoch !== binding.daemonEpoch || record.listenerEpoch !== binding.listenerEpoch || record.listener !== binding.listener) await this.remove(record.credentialId);
      else kept.push(record);
    }
    return kept;
  }

  /** The stored private key still matches its public key (detects a swapped key). */
  async keyMatches(record: CredentialRecord): Promise<boolean> {
    try {
      return await signsWithPublicKey(record.privateKey, record.publicKey);
    } catch {
      return false;
    }
  }
}

let shared: CredentialStore | null = null;

/** The document's credential store (IndexedDB when available). */
export function credentialStore(): CredentialStore {
  shared ??= new CredentialStore();
  return shared;
}

/** Tests only: replace the shared store. */
export function setCredentialStoreForTests(store: CredentialStore | null): void {
  shared = store;
}
