import { describe, expect, it } from 'vitest';
import { PASTE_END, PASTE_START, preparePaste, sanitizePasteText } from '@/terminal/input/paste';

const text = (b: Uint8Array) => new TextDecoder().decode(b);

describe('preparePaste', () => {
  it('wraps in bracketed-paste markers when the agent enabled mode 2004', () => {
    const p = preparePaste('hello\nworld', true);
    expect(p.ok && text(p.bytes)).toBe(`${PASTE_START}hello\rworld${PASTE_END}`);
  });
  it('sends plain text otherwise, with CR line endings', () => {
    const p = preparePaste('a\r\nb\nc', false);
    expect(p.ok && text(p.bytes)).toBe('a\rb\rc');
  });
  it('removes ESC so a payload cannot end bracketed paste early or inject sequences', () => {
    const p = preparePaste(`x${PASTE_END}rm -rf ~\n\x1b]52;c;AAAA\x07`, true);
    expect(p.ok).toBe(true);
    const out = p.ok ? text(p.bytes) : '';
    expect(out.startsWith(PASTE_START) && out.endsWith(PASTE_END)).toBe(true);
    expect(out.slice(PASTE_START.length, -PASTE_END.length)).not.toContain('\x1b');
  });
  it('keeps tabs and unicode, drops other controls and C1', () => {
    expect(sanitizePasteText('a\tb\x00c\x07d\x9be中')).toBe('a\tbcde中');
  });
  it('refuses more than 1 MiB with a visible message', () => {
    const big = preparePaste('x'.repeat(1024 * 1024 + 1), false);
    expect(big).toEqual({ ok: false, message: 'Paste is larger than 1 MiB and was not sent.' });
    // Multi-byte text over the byte limit is refused after encoding too.
    expect(preparePaste('中'.repeat(400_000), false).ok).toBe(false);
    expect(preparePaste('x'.repeat(1024 * 1024 - 100), false).ok).toBe(true);
  });
  it('refuses an empty paste', () => expect(preparePaste('\x1b\x00', false).ok).toBe(false));
});
