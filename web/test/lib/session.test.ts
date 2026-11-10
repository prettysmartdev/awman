import { describe, expect, it } from 'vitest';
import { RESUME_KEY, VIEW_KEY, clearResume, readResume, readViewPrefs, writeResume, writeViewPrefs, type StorageLike } from '@/lib/state/session';

function memoryStorage(): StorageLike & { data: Map<string, string> } {
  const data = new Map<string, string>();
  return { data, getItem: k => data.get(k) ?? null, setItem: (k, v) => void data.set(k, v), removeItem: k => void data.delete(k) };
}

describe('session storage policy', () => {
  it('round-trips only the resume ids', () => {
    const storage = memoryStorage();
    writeResume({ instanceId: 'ins_abc', frontendId: 'frn_def', tabId: 'tab_1', launchContext: 'lc_9' }, storage);
    expect([...storage.data.keys()]).toEqual([RESUME_KEY]);
    expect(readResume(storage)).toEqual({ instanceId: 'ins_abc', frontendId: 'frn_def', tabId: 'tab_1', launchContext: 'lc_9' });
    clearResume(storage);
    expect(readResume(storage)).toBeNull();
  });

  it('never stores or accepts credentials in place of ids', () => {
    const storage = memoryStorage();
    writeResume({ instanceId: 'awb_secret', frontendId: 'frn_1', tabId: null, launchContext: null }, storage);
    expect(storage.data.size).toBe(0);
    storage.data.set(RESUME_KEY, JSON.stringify({ instanceId: 'ins_1', frontendId: 'awt_ticket', token: 'awb_x' }));
    expect(readResume(storage)).toBeNull();
  });

  it('ignores malformed and oversized values', () => {
    const storage = memoryStorage();
    storage.data.set(RESUME_KEY, '{not json');
    expect(readResume(storage)).toBeNull();
    storage.data.set(RESUME_KEY, JSON.stringify({ instanceId: 'ins_1', frontendId: 'frn_1', pad: 'x'.repeat(5000) }));
    expect(readResume(storage)).toBeNull();
  });

  it('sanitises view prefs', () => {
    const storage = memoryStorage();
    writeViewPrefs({ panelSizes: { git: 320, 'bad key!': 1, huge: 1e9 }, focus: 'composer' }, storage);
    expect(JSON.parse(storage.data.get(VIEW_KEY)!)).toEqual({ panelSizes: { git: 320 }, focus: 'composer' });
    expect(readViewPrefs(storage)).toEqual({ panelSizes: { git: 320 }, focus: 'composer' });
  });

  it('works without storage (privacy mode)', () => {
    expect(readResume(null)).toBeNull();
    expect(() => writeResume({ instanceId: 'ins_1', frontendId: 'frn_1', tabId: null, launchContext: null }, null)).not.toThrow();
    const throwing: StorageLike = {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
      removeItem: () => undefined,
    };
    expect(readResume(throwing)).toBeNull();
    expect(() => writeViewPrefs({ panelSizes: {}, focus: null }, throwing)).not.toThrow();
  });
});
