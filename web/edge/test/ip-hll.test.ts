import { describe, expect, it } from 'vitest';
import { canonicalIp } from '../src/ip';
import { addDigest, estimate, HLL_REGISTERS, newSketch } from '../src/hll';
import { navigationDigests } from '../src/privacy';

const secret = 'a-test-key-with-at-least-32-bytes-of-entropy';
describe('strict full-address canonicalization', () => {
  it.each([
    ['192.0.2.1', '192.0.2.1'], ['0.0.0.0', '0.0.0.0'], ['255.255.255.255', '255.255.255.255'],
    ['2001:DB8::A', '2001:0db8:0000:0000:0000:0000:0000:000a'],
    ['2001:0db8:0:0:0:0:0:000A', '2001:0db8:0000:0000:0000:0000:0000:000a'],
    ['::', '0000:0000:0000:0000:0000:0000:0000:0000'],
    ['::1', '0000:0000:0000:0000:0000:0000:0000:0001'],
    ['::ffff:192.0.2.1', '0000:0000:0000:0000:0000:ffff:c000:0201'],
    ['::ffff:c000:201', '0000:0000:0000:0000:0000:ffff:c000:0201'],
    ['1:2:3:4:5:6:7:8', '0001:0002:0003:0004:0005:0006:0007:0008'],
  ])('canonicalizes %s', (input, expected) => expect(canonicalIp(input)).toBe(expected));
  it.each([
    null, '', '192.168.1', '192.168.001.1', '0x7f000001', '127.1', '3232235777', '256.0.0.1',
    '1.2.3.4:80', '1.2.3.4, 5.6.7.8', ' 1.2.3.4', '[::1]', 'fe80::1%eth0', ':::1',
    '1::2::3', '1:2:3:4:5:6:7', '1:2:3:4:5:6:7:8:9', '1:2:3:4:5:6:7:8::',
    '::gggg', '::ffff:192.00.2.1', '1.2.3.4::', '1:2:3:4:5:6:7:', ':1:2:3:4:5:6:7',
  ])('rejects %s', input => expect(canonicalIp(input)).toBeNull());

  it('HMACs canonical addresses, with the exact domain and period separators', async () => {
    const periods = { day: '2099-01-31', month: '2099-01' };
    const first = await navigationDigests('2001:db8::a', secret, periods);
    const expanded = await navigationDigests('2001:0DB8:0:0:0:0:0:000a', secret, periods);
    expect(first).toEqual(expanded);
    const key = await crypto.subtle.importKey('raw', new TextEncoder().encode(secret), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
    const expected = await crypto.subtle.sign('HMAC', key, new TextEncoder().encode('day|2099-01-31|2001:0db8:0000:0000:0000:0000:0000:000a'));
    expect(first.day).toEqual(new Uint8Array(expected));
    expect(first.month).not.toEqual(first.day);
    const nextDay = await navigationDigests('2001:db8::a', secret, { day: '2099-02-01', month: '2099-02' });
    expect(nextDay.day).not.toEqual(first.day);
    expect(nextDay.month).not.toEqual(first.month);
  });
});

describe('bounded HLL p=12 with linear counting', () => {
  it('has fixed memory, deduplicates, and handles zero and rank extremes', () => {
    const sketch = newSketch();
    expect(sketch.length).toBe(HLL_REGISTERS);
    expect(estimate(sketch)).toBe(0);
    const digest = new Uint8Array(32).fill(0xff);
    addDigest(sketch, digest);
    const once = sketch.slice();
    for (let i = 0; i < 100; i++) addDigest(sketch, digest);
    expect(sketch).toEqual(once);
    expect(estimate(sketch)).toBeCloseTo(1, 2);
    addDigest(sketch, new Uint8Array(32));
    expect(sketch[0]).toBe(245);
    expect(() => addDigest(sketch, new Uint8Array(31))).toThrow('Invalid sketch input');
  });
  it.each([1, 10, 100, 1_000, 10_000, 100_000])('estimates %i distinct uniform hashes within 6%% over three seeds', async count => {
    for (const seed of [17, 1027, 4931]) {
      const sketch = newSketch();
      // Deterministic SplitMix-style 32-bit mixer produces uniform test hashes;
      // crypto HMAC domain separation is independently tested above.
      let state = seed;
      const word = () => {
        state = (state + 0x9e3779b9) | 0;
        let x = state ^ (state >>> 16);
        x = Math.imul(x, 0x21f0aaad);
        x ^= x >>> 15;
        x = Math.imul(x, 0x735a2d97);
        return (x ^ (x >>> 15)) >>> 0;
      };
      for (let i = 0; i < count; i++) {
        const digest = new Uint8Array(32);
        const view = new DataView(digest.buffer);
        for (let offset = 0; offset < 32; offset += 4) view.setUint32(offset, word());
        addDigest(sketch, digest);
      }
      expect(Math.abs(estimate(sketch) - count) / count).toBeLessThan(0.06);
    }
  });
});
