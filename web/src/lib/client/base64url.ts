// Unpadded, canonical base64url (RFC 4648 §5) for the browser reauthorization
// wire format (WI 0160 C2/C3). Decoding refuses padding, the standard
// alphabet, whitespace and non-canonical trailing bits, so one byte string
// has exactly one accepted text form.
import { decodeBase64, encodeBase64 } from './base64';

const B64U_RE = /^[A-Za-z0-9_-]*$/;

export function encodeBase64Url(bytes: Uint8Array): string {
  return encodeBase64(bytes).replace(/=+$/, '').replace(/\+/g, '-').replace(/\//g, '_');
}

/** Decode canonical unpadded base64url; null for anything else. */
export function decodeBase64Url(text: string): Uint8Array | null {
  if (typeof text !== 'string' || !B64U_RE.test(text) || text.length % 4 === 1) return null;
  let bytes: Uint8Array;
  try {
    const standard = text.replace(/-/g, '+').replace(/_/g, '/');
    bytes = decodeBase64(standard + '='.repeat((4 - (standard.length % 4)) % 4));
  } catch {
    return null;
  }
  // Canonical form only: non-zero trailing bits re-encode differently.
  return encodeBase64Url(bytes) === text ? bytes : null;
}

/** True when `text` is canonical base64url of exactly `bytes` bytes. */
export function isBase64UrlOf(text: unknown, bytes: number): text is string {
  if (typeof text !== 'string' || text.length !== Math.ceil((bytes * 4) / 3)) return false;
  const decoded = decodeBase64Url(text);
  return decoded !== null && decoded.length === bytes;
}

export function randomBase64Url(bytes: number): string {
  const buffer = new Uint8Array(bytes);
  crypto.getRandomValues(buffer);
  return encodeBase64Url(buffer);
}
