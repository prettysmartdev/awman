// Paste → PTY bytes. Pasted text is user-supplied but may carry hidden
// control sequences (paste-jacking), so ESC and C1 controls are removed in
// both modes; that also stops a payload from closing bracketed paste early
// with its own `ESC [ 201 ~`. Line endings become CR, as a terminal sends.
// The connection chunks at 64 KiB; anything over 1 MiB is refused here with
// a visible message instead of being truncated.
import { TERMINAL_PASTE_MAX_BYTES } from '@/lib/client/types';

const encoder = new TextEncoder();
export const PASTE_START = '\x1b[200~';
export const PASTE_END = '\x1b[201~';

export type PreparedPaste = { ok: true; bytes: Uint8Array } | { ok: false; message: string };

export function sanitizePasteText(text: string): string {
  // eslint-disable-next-line no-control-regex -- removing control characters from pasted text
  return text.replace(/\r\n?/g, '\r').replace(/\n/g, '\r').replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]/g, '');
}

export function preparePaste(text: string, bracketed: boolean, limit = TERMINAL_PASTE_MAX_BYTES): PreparedPaste {
  // Cheap bound before encoding: UTF-8 is at most 3 bytes per UTF-16 unit.
  if (text.length > limit) return { ok: false, message: 'Paste is larger than 1 MiB and was not sent.' };
  const clean = sanitizePasteText(text);
  if (clean.length === 0) return { ok: false, message: 'Nothing to paste.' };
  const bytes = encoder.encode(bracketed ? `${PASTE_START}${clean}${PASTE_END}` : clean);
  if (bytes.length > limit) return { ok: false, message: 'Paste is larger than 1 MiB and was not sent.' };
  return { ok: true, bytes };
}
