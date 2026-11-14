import { canonicalIp } from './ip';
import type { Periods } from './periods';
export interface Digests { day: Uint8Array | null; month: Uint8Array | null }
const encoder = new TextEncoder();

export async function navigationDigests(ipHeader: string | null, secret: string, periods: Periods): Promise<Digests> {
  const ip = canonicalIp(ipHeader);
  if (!ip) return { day: null, month: null };
  if (secret.length < 32) throw new Error('Usage secret unavailable');
  const key = await crypto.subtle.importKey('raw', encoder.encode(secret), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  const [day, month] = await Promise.all([
    crypto.subtle.sign('HMAC', key, encoder.encode(`day|${periods.day}|${ip}`)),
    crypto.subtle.sign('HMAC', key, encoder.encode(`month|${periods.month}|${ip}`)),
  ]);
  // Only the two transient digests leave this function. No key/IP cache.
  return { day: new Uint8Array(day), month: new Uint8Array(month) };
}

export async function ownerAuthorized(header: string | null, secret: string): Promise<boolean> {
  if (!secret || secret.length < 32 || !header?.startsWith('Bearer ') || header.length > 512) return false;
  // Equal-sized SHA-256 digests avoid comparing the token with an early exit.
  const [actual, expected] = await Promise.all([
    crypto.subtle.digest('SHA-256', encoder.encode(header.slice(7))),
    crypto.subtle.digest('SHA-256', encoder.encode(secret)),
  ]);
  return crypto.subtle.timingSafeEqual(actual, expected);
}
