// Keyboard and mouse → PTY bytes for awman.terminal.v1 `input` frames.
//
// encodeKey is a port of src/frontend/tui/key_handler.rs::key_to_bytes so the
// browser sends exactly what the TUI sends for the same key. Where the TUI
// sends nothing or ignores a terminal mode, the browser follows xterm:
// - DECCKM (application cursor keys): arrows/Home/End as SS3 (`ESC O A`);
// - Shift+Tab (crossterm BackTab, dropped by the TUI) → `ESC [ Z`;
// - Alt+<printable> → ESC prefix (meta sends escape);
// - Ctrl+Space/@ → NUL, Ctrl+[ ] ^ _ → ESC, GS, RS, US; Insert → `ESC [ 2 ~`.
// F-keys keep the TUI's `ESC [ n ~` form (parity, see the fixtures README).
//
// encodeMouse is X10 / normal (1000) / button-event (1002) / any-event (1003)
// tracking with the default X10 byte encoding or SGR (1006); wheel buttons
// 64/65 match src/frontend/tui/mouse.rs::encode_mouse_scroll.
import type { EncodeKey, EncodeMouse, KeyModes, MouseLike, MouseMode } from '../contract';

const encoder = new TextEncoder();
const ESC = 0x1b;

/** The subset of KeyboardEvent the encoder reads (tests pass plain objects). */
export type KeyLike = Pick<KeyboardEvent, 'key' | 'ctrlKey' | 'altKey' | 'shiftKey' | 'metaKey'> & { isComposing?: boolean };

function bytes(text: string): Uint8Array {
  return encoder.encode(text);
}

function cursorKey(final: string, modes: KeyModes): Uint8Array {
  return bytes(modes.appCursor ? `\x1bO${final}` : `\x1b[${final}`);
}

const CTRL_SYMBOLS: Record<string, number> = { ' ': 0x00, '@': 0x00, '2': 0x00, '[': 0x1b, ']': 0x1d, '^': 0x1e, '6': 0x1e, '_': 0x1f, '-': 0x1f, '/': 0x1f };

/** One user-perceived character (so "é" or "👍🏽" count, "Enter" does not). */
function isPrintable(key: string): boolean {
  if (key.length === 0) return false;
  if (key.length === 1) return key >= ' ' && key !== '\x7f';
  // Multi-unit keys are printable only when they form a single grapheme.
  const segmenter = typeof Intl !== 'undefined' && 'Segmenter' in Intl ? new Intl.Segmenter(undefined, { granularity: 'grapheme' }) : null;
  if (!segmenter) return [...key].length === 1;
  const iterator = segmenter.segment(key)[Symbol.iterator]();
  const first = iterator.next();
  return !first.done && first.value.segment === key;
}

function withAlt(alt: boolean, data: Uint8Array): Uint8Array {
  if (!alt) return data;
  const out = new Uint8Array(data.length + 1);
  out[0] = ESC;
  out.set(data, 1);
  return out;
}

export function encodeKeyLike(event: KeyLike, modes: KeyModes): Uint8Array | null {
  if (event.isComposing || event.metaKey) return null;
  const { key, ctrlKey: ctrl, altKey: alt, shiftKey: shift } = event;
  switch (key) {
    case 'Enter':
      return withAlt(alt, bytes('\r'));
    case 'Backspace':
      return withAlt(alt, bytes('\x7f'));
    case 'Tab':
      return shift ? bytes('\x1b[Z') : withAlt(alt, bytes('\t'));
    case 'Escape':
      return bytes('\x1b');
    case 'ArrowUp':
      return cursorKey('A', modes);
    case 'ArrowDown':
      return cursorKey('B', modes);
    case 'ArrowRight':
      return cursorKey('C', modes);
    case 'ArrowLeft':
      return cursorKey('D', modes);
    case 'Home':
      return cursorKey('H', modes);
    case 'End':
      return cursorKey('F', modes);
    case 'PageUp':
      return bytes('\x1b[5~');
    case 'PageDown':
      return bytes('\x1b[6~');
    case 'Delete':
      return bytes('\x1b[3~');
    case 'Insert':
      return bytes('\x1b[2~');
  }
  const fkey = /^F([1-9]|1[0-2])$/.exec(key);
  if (fkey) return bytes(`\x1b[${fkey[1]}~`);
  if (!isPrintable(key)) return null; // Shift, Dead, Process, Unidentified, media keys…
  if (ctrl) {
    const lower = key.toLowerCase();
    if (lower.length === 1 && lower >= 'a' && lower <= 'z') return withAlt(alt, Uint8Array.of(lower.charCodeAt(0) - 0x60));
    const symbol = CTRL_SYMBOLS[key];
    if (symbol !== undefined) return withAlt(alt, Uint8Array.of(symbol));
    // The TUI sends the character itself for other Ctrl chords.
  }
  return withAlt(alt, bytes(key));
}

export const encodeKey: EncodeKey = (event, modes) => encodeKeyLike(event, modes);

/** Wheel and alternate-scroll (DECSET 1007-style) arrows: three per tick, like the TUI. */
export function encodeAltScroll(up: boolean, appCursor: boolean): Uint8Array {
  const arrow = appCursor ? (up ? '\x1bOA' : '\x1bOB') : up ? '\x1b[A' : '\x1b[B';
  return bytes(arrow.repeat(3));
}

function modifierBits(event: MouseLike): number {
  return (event.shift ? 4 : 0) | (event.alt ? 8 : 0) | (event.ctrl ? 16 : 0);
}

export const encodeMouse: EncodeMouse = (event: MouseLike, mode: MouseMode) => {
  if (mode === 'none') return null;
  if (!Number.isFinite(event.col) || !Number.isFinite(event.row) || event.col < 1 || event.row < 1) return null;
  const col = Math.floor(event.col);
  const row = Math.floor(event.row);
  let code: number;
  let release = false;
  switch (event.kind) {
    case 'wheel':
      if (mode === 'x10') return null;
      code = (event.wheelDelta ?? 0) < 0 ? 64 : 65;
      break;
    case 'down':
      if (event.button === 3) return null;
      code = event.button;
      break;
    case 'up':
      if (mode === 'x10') return null;
      release = true;
      code = event.sgr ? event.button : 3;
      if (event.sgr && event.button === 3) return null;
      break;
    case 'move':
      if (mode === 'any') code = 32 + event.button;
      else if (mode === 'button' && event.button !== 3) code = 32 + event.button;
      else return null;
      break;
  }
  if (mode !== 'x10') code |= modifierBits(event);
  if (event.sgr) return bytes(`\x1b[<${code};${col};${row}${release ? 'm' : 'M'}`);
  // Default encoding: one byte per value offset by 32; coordinates clamp at 223 (byte 255).
  return Uint8Array.of(ESC, 0x5b, 0x4d, 32 + code, 32 + Math.min(col, 223), 32 + Math.min(row, 223));
};

/** Map a core's DECSET tracking mode to the contract's MouseMode. */
export function mouseModeOf(tracking: number): MouseMode {
  switch (tracking) {
    case 9:
      return 'x10';
    case 1000:
      return 'normal';
    case 1002:
      return 'button';
    case 1003:
      return 'any';
    default:
      return 'none';
  }
}
