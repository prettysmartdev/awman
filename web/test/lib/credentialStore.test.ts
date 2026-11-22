import { describe, expect, it } from 'vitest';
import { CredentialStore, CredentialStoreFull, MAX_CREDENTIALS, memoryBackend, validateRecord, type CredentialRecord } from '@/lib/client/credentialStore';

const ORIGIN = 'http://127.0.0.1:3000';
const BASE = 'http://127.0.0.1:9877';
const BINDING = { listener: 'loopback', daemonEpoch: 'Gs9c1yhW-SA6P6HLQUsNew', listenerEpoch: 'C0tOZXEBuALBXtZAsgmDCg' };

function store(backend = memoryBackend(), origin = ORIGIN) {
  return { backend, store: new CredentialStore(backend, { origin, daemonBase: BASE }) };
}

describe('credential store (C1, C4)', () => {
  it('creates a fresh, private, non-extractable, sign-only key that round trips and signs', async () => {
    const { backend, store: s } = store();
    const candidate = await s.createCandidate(BINDING);
    expect(candidate).not.toBeNull();
    const stored = backend.data.get(candidate!.record.credentialId) as CredentialRecord;
    expect(Object.keys(stored).sort()).toEqual(['credentialId', 'daemonBase', 'daemonEpoch', 'expiresAt', 'grantId', 'linkExpiresAt', 'listener', 'listenerEpoch', 'origin', 'privateKey', 'publicKey', 'version']);
    expect(stored).toMatchObject({ version: 1, origin: ORIGIN, daemonBase: BASE, ...BINDING, grantId: null, expiresAt: null, linkExpiresAt: null });
    expect(stored.privateKey.type).toBe('private');
    expect(stored.privateKey.extractable).toBe(false);
    expect(stored.privateKey.usages).toEqual(['sign']);
    await expect(crypto.subtle.exportKey('pkcs8', stored.privateKey)).rejects.toThrow();
    await expect(crypto.subtle.exportKey('jwk', stored.privateKey)).rejects.toThrow();
    expect(candidate!.publicKey).toHaveLength(87);
    expect(await s.keyMatches(stored)).toBe(true);
    // Every link attempt gets a new key and ID.
    const second = await s.createCandidate(BINDING);
    expect(second!.record.credentialId).not.toBe(candidate!.record.credentialId);
    expect(second!.publicKey).not.toBe(candidate!.publicKey);
  });

  it('falls back to memory-only (null) when storage cannot keep the key, and leaves nothing behind', async () => {
    const backend = memoryBackend();
    backend.put = async () => {
      throw new DOMException('quota', 'QuotaExceededError');
    };
    expect(await new CredentialStore(backend, { origin: ORIGIN, daemonBase: BASE }).createCandidate(BINDING)).toBeNull();
    expect(await new CredentialStore(null, { origin: ORIGIN, daemonBase: BASE }).createCandidate(BINDING)).toBeNull();
    const corrupting = memoryBackend();
    corrupting.get = async () => ({ broken: true });
    expect(await new CredentialStore(corrupting, { origin: ORIGIN, daemonBase: BASE }).createCandidate(BINDING)).toBeNull();
    expect(corrupting.data.size).toBe(0);
  });

  it('drops tampered records on read: extra field, extractable key, wrong usages, bad id, raw token', async () => {
    const { backend, store: s } = store();
    const good = (await s.createCandidate(BINDING))!.record;
    const extractable = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, ['sign', 'verify']);
    const tampered: Record<string, unknown>[] = [
      { ...good, credentialId: 'A'.repeat(42) + 'E', token: 'awb_x' },
      { ...good, credentialId: 'B'.repeat(42) + 'E', privateKey: extractable.privateKey },
      { ...good, credentialId: 'C'.repeat(42) + 'E', privateKey: extractable.publicKey },
      { ...good, credentialId: 'D'.repeat(42) + 'E', publicKey: 'A'.repeat(87) },
      { ...good, credentialId: 'E'.repeat(42) + 'E', grantId: 'awb_' + 'A'.repeat(43) },
      { ...good, credentialId: 'F'.repeat(42) + 'E', version: 2 },
    ];
    for (const record of tampered) backend.data.set(record.credentialId as string, record);
    const loaded = await s.load();
    expect(loaded.usable.map(r => r.credentialId)).toEqual([good.credentialId]);
    expect([...backend.data.keys()]).toEqual([good.credentialId]);
    for (const record of tampered) expect(validateRecord(record)).toBeNull();
  });

  it('reads only records for this page origin and daemon base, and reports a foreign-origin record', async () => {
    const backend = memoryBackend();
    const other = new CredentialStore(backend, { origin: 'http://localhost:3000', daemonBase: BASE });
    await other.createCandidate(BINDING);
    const loaded = await store(backend).store.load();
    expect(loaded.usable).toEqual([]);
    expect(loaded.foreignOrigin).toBe(true);
  });

  it('updates only enumerated public metadata, atomically', async () => {
    const { backend, store: s } = store();
    const record = (await s.createCandidate(BINDING))!.record;
    const updated = await s.update(record.credentialId, { grantId: 'brw_0123', expiresAt: '2999-01-01T00:00:00.000Z', ...({ token: 'awb_secret' } as object) });
    expect(updated).toMatchObject({ grantId: 'brw_0123', expiresAt: '2999-01-01T00:00:00.000Z' });
    expect(JSON.stringify({ ...(backend.data.get(record.credentialId) as object), privateKey: null })).not.toContain('awb_');
    expect(await s.update(record.credentialId, { grantId: 'not a grant id' })).toBeNull();
  });

  it('caps saved authorizations at eight without silently evicting one', async () => {
    const { backend, store: s } = store();
    for (let i = 0; i < MAX_CREDENTIALS; i++) await s.createCandidate(BINDING);
    await expect(s.createCandidate(BINDING)).rejects.toBeInstanceOf(CredentialStoreFull);
    expect(backend.data.size).toBe(MAX_CREDENTIALS);
  });

  it('purges records of other daemon or listener generations before linking', async () => {
    const { store: s } = store();
    await s.createCandidate({ ...BINDING, daemonEpoch: 'AAAAAAAAAAAAAAAAAAAAAA' });
    await s.createCandidate({ ...BINDING, listenerEpoch: 'AAAAAAAAAAAAAAAAAAAAAA' });
    const current = (await s.createCandidate(BINDING))!.record;
    expect((await s.purgeOtherGenerations(BINDING)).map(r => r.credentialId)).toEqual([current.credentialId]);
    expect((await s.load()).usable).toHaveLength(1);
  });
});
