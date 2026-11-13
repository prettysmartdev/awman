// encodeKey: table ported from src/frontend/tui/key_handler.rs::key_to_bytes
// (TUI parity), plus the xterm cases the TUI drops (DECCKM, BackTab, Alt).
// encodeMouse: X10/normal/button/any with default and SGR encodings; wheel
// rows mirror src/frontend/tui/mouse.rs tests.
import { describe, expect, it } from 'vitest';
import { encodeKey, encodeMouse } from '@/terminal/contract';
import { encodeAltScroll, encodeKeyLike, mouseModeOf, type KeyLike } from '@/terminal/input/encoder';
import type { MouseLike } from '@/terminal/contract';

const k = (key: string, mods: Partial<KeyLike> = {}): KeyLike => ({ key, ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, ...mods });
const NORMAL = { appCursor: false, bracketedPaste: false };
const APP = { appCursor: true, bracketedPaste: false };
const str = (bytes: Uint8Array | null) => (bytes === null ? null : Buffer.from(bytes).toString('latin1'));

describe('encodeKey — key_handler::key_to_bytes parity', () => {
  const table: [string, KeyLike, string][] = [
    ['Char a', k('a'), 'a'],
    ['Char A (shift)', k('A', { shiftKey: true }), 'A'],
    ['Char space', k(' '), ' '],
    ['Char é (UTF-8)', k('é'), '\xc3\xa9'],
    ['Char 中 (UTF-8)', k('中'), '\xe4\xb8\xad'],
    ['Ctrl+a', k('a', { ctrlKey: true }), '\x01'],
    ['Ctrl+c', k('c', { ctrlKey: true }), '\x03'],
    ['Ctrl+C (shift)', k('C', { ctrlKey: true, shiftKey: true }), '\x03'],
    ['Ctrl+z', k('z', { ctrlKey: true }), '\x1a'],
    ['Ctrl+1 (no control mapping → char)', k('1', { ctrlKey: true }), '1'],
    ['Enter', k('Enter'), '\r'],
    ['Backspace', k('Backspace'), '\x7f'],
    ['Tab', k('Tab'), '\t'],
    ['Esc', k('Escape'), '\x1b'],
    ['Up', k('ArrowUp'), '\x1b[A'],
    ['Down', k('ArrowDown'), '\x1b[B'],
    ['Right', k('ArrowRight'), '\x1b[C'],
    ['Left', k('ArrowLeft'), '\x1b[D'],
    ['Home', k('Home'), '\x1b[H'],
    ['End', k('End'), '\x1b[F'],
    ['PageUp', k('PageUp'), '\x1b[5~'],
    ['PageDown', k('PageDown'), '\x1b[6~'],
    ['Delete', k('Delete'), '\x1b[3~'],
    ['F1', k('F1'), '\x1b[1~'],
    ['F5', k('F5'), '\x1b[5~'],
    ['F12', k('F12'), '\x1b[12~'],
  ];
  for (const [label, key, want] of table) {
    it(label, () => expect(str(encodeKeyLike(key, NORMAL))).toBe(want));
  }

  it('sends nothing for modifiers, dead keys, IME and Cmd chords', () => {
    for (const key of [k('Shift', { shiftKey: true }), k('Control', { ctrlKey: true }), k('Alt', { altKey: true }), k('Dead'), k('Process'), k('Unidentified'), k('CapsLock'), k('AudioVolumeUp')]) {
      expect(encodeKeyLike(key, NORMAL)).toBeNull();
    }
    expect(encodeKeyLike(k('c', { metaKey: true }), NORMAL)).toBeNull();
    expect(encodeKeyLike({ ...k('a'), isComposing: true }, NORMAL)).toBeNull();
  });

  it('works on real KeyboardEvent-shaped input through the contract export', () => {
    expect(str(encodeKey({ key: 'x', ctrlKey: false, altKey: false, shiftKey: false, metaKey: false } as KeyboardEvent, NORMAL))).toBe('x');
  });
});

describe('encodeKey — xterm behaviour where the TUI is lossy', () => {
  it('application cursor mode (DECCKM) uses SS3 for arrows, Home and End', () => {
    expect(str(encodeKeyLike(k('ArrowUp'), APP))).toBe('\x1bOA');
    expect(str(encodeKeyLike(k('ArrowDown'), APP))).toBe('\x1bOB');
    expect(str(encodeKeyLike(k('ArrowRight'), APP))).toBe('\x1bOC');
    expect(str(encodeKeyLike(k('ArrowLeft'), APP))).toBe('\x1bOD');
    expect(str(encodeKeyLike(k('Home'), APP))).toBe('\x1bOH');
    expect(str(encodeKeyLike(k('End'), APP))).toBe('\x1bOF');
    expect(str(encodeKeyLike(k('PageUp'), APP))).toBe('\x1b[5~');
  });
  it('Shift+Tab is BackTab', () => expect(str(encodeKeyLike(k('Tab', { shiftKey: true }), NORMAL))).toBe('\x1b[Z'));
  it('Alt prefixes ESC', () => {
    expect(str(encodeKeyLike(k('b', { altKey: true }), NORMAL))).toBe('\x1bb');
    expect(str(encodeKeyLike(k('Enter', { altKey: true }), NORMAL))).toBe('\x1b\r');
  });
  it('Ctrl symbols', () => {
    expect(str(encodeKeyLike(k(' ', { ctrlKey: true }), NORMAL))).toBe('\x00');
    expect(str(encodeKeyLike(k('[', { ctrlKey: true }), NORMAL))).toBe('\x1b');
    expect(str(encodeKeyLike(k(']', { ctrlKey: true }), NORMAL))).toBe('\x1d');
    expect(str(encodeKeyLike(k('_', { ctrlKey: true }), NORMAL))).toBe('\x1f');
  });
  it('Insert', () => expect(str(encodeKeyLike(k('Insert'), NORMAL))).toBe('\x1b[2~'));
  it('a single multi-code-point grapheme is printable', () => expect(str(encodeKeyLike(k('👍🏽'), NORMAL))).toBe(Buffer.from('👍🏽').toString('latin1')));
});

describe('encodeMouse', () => {
  const m = (over: Partial<MouseLike>): MouseLike => ({ kind: 'down', button: 0, col: 5, row: 3, shift: false, alt: false, ctrl: false, sgr: false, ...over });
  const bytes = (b: Uint8Array | null) => (b ? [...b] : null);

  it('wheel matches mouse.rs encode_mouse_scroll (SGR and default)', () => {
    expect(str(encodeMouse(m({ kind: 'wheel', wheelDelta: -1, sgr: true }), 'normal'))).toBe('\x1b[<64;5;3M');
    expect(str(encodeMouse(m({ kind: 'wheel', wheelDelta: 1, sgr: true }), 'normal'))).toBe('\x1b[<65;5;3M');
    expect(bytes(encodeMouse(m({ kind: 'wheel', wheelDelta: -1 }), 'normal'))).toEqual([0x1b, 0x5b, 0x4d, 96, 37, 35]);
    expect(bytes(encodeMouse(m({ kind: 'wheel', wheelDelta: 1 }), 'normal'))).toEqual([0x1b, 0x5b, 0x4d, 97, 37, 35]);
  });
  it('default encoding clamps large coordinates at byte 255', () => {
    expect(bytes(encodeMouse(m({ kind: 'wheel', wheelDelta: -1, col: 301, row: 501 }), 'normal'))).toEqual([0x1b, 0x5b, 0x4d, 96, 255, 255]);
  });
  it('press/release: SGR keeps the button and uses m; default reports release as 3', () => {
    expect(str(encodeMouse(m({ kind: 'down', button: 0, sgr: true }), 'normal'))).toBe('\x1b[<0;5;3M');
    expect(str(encodeMouse(m({ kind: 'up', button: 0, sgr: true }), 'normal'))).toBe('\x1b[<0;5;3m');
    expect(str(encodeMouse(m({ kind: 'down', button: 2, sgr: true }), 'normal'))).toBe('\x1b[<2;5;3M');
    expect(bytes(encodeMouse(m({ kind: 'down', button: 0 }), 'normal'))).toEqual([0x1b, 0x5b, 0x4d, 32, 37, 35]);
    expect(bytes(encodeMouse(m({ kind: 'up', button: 0 }), 'normal'))).toEqual([0x1b, 0x5b, 0x4d, 35, 37, 35]);
  });
  it('modifiers add shift 4, meta 8, ctrl 16', () => {
    expect(str(encodeMouse(m({ shift: true, alt: true, ctrl: true, sgr: true }), 'normal'))).toBe('\x1b[<28;5;3M');
  });
  it('motion is reported only in button (with a button held) and any modes', () => {
    expect(encodeMouse(m({ kind: 'move', button: 3, sgr: true }), 'normal')).toBeNull();
    expect(encodeMouse(m({ kind: 'move', button: 3, sgr: true }), 'button')).toBeNull();
    expect(str(encodeMouse(m({ kind: 'move', button: 0, sgr: true }), 'button'))).toBe('\x1b[<32;5;3M');
    expect(str(encodeMouse(m({ kind: 'move', button: 3, sgr: true }), 'any'))).toBe('\x1b[<35;5;3M');
  });
  it('X10 mode reports presses only, without modifiers', () => {
    expect(bytes(encodeMouse(m({ kind: 'down', button: 1, ctrl: true }), 'x10'))).toEqual([0x1b, 0x5b, 0x4d, 33, 37, 35]);
    expect(encodeMouse(m({ kind: 'up' }), 'x10')).toBeNull();
    expect(encodeMouse(m({ kind: 'wheel', wheelDelta: 1 }), 'x10')).toBeNull();
  });
  it('nothing when tracking is off or coordinates are invalid', () => {
    expect(encodeMouse(m({}), 'none')).toBeNull();
    expect(encodeMouse(m({ col: 0 }), 'normal')).toBeNull();
    expect(encodeMouse(m({ row: Number.NaN }), 'normal')).toBeNull();
  });
  it('maps DECSET tracking modes', () => {
    expect([0, 9, 1000, 1002, 1003, 1005].map(mouseModeOf)).toEqual(['none', 'x10', 'normal', 'button', 'any', 'none']);
  });
  it('alternate scroll sends three arrows (mouse.rs encode_alt_scroll)', () => {
    expect(str(encodeAltScroll(true, false))).toBe('\x1b[A\x1b[A\x1b[A');
    expect(str(encodeAltScroll(false, true))).toBe('\x1bOB\x1bOB\x1bOB');
  });
});
