// @vitest-environment jsdom
// Untrusted agent output (design §5.5, WI 0159 §3): a seeded fuzz of 10 000
// ANSI/OSC/DCS/APC sequences — including crafted OSC 52/8/7/0/2, query
// sequences, HTML and invalid UTF-8 — must cause no clipboard access, no
// navigation, no window.open, no downloads, no input back to the PTY, and no
// DOM other than plain rows of text spans.
import { act, cleanup, render, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { TerminalSession } from '@/terminal/adapter/session';
import { createTerminalView } from '@/terminal/TerminalView';
import type { DetectedLink } from '@/terminal/contract';
import { MAX_TITLE_CHARS, findUrls, isUnsafeCodePoint, sanitizeTitle, vetLink } from '@/terminal/security/sanitize';
import { FakeConnection, nodeCoreFactory, syncSchedule } from './helpers';

// Deterministic PRNG (mulberry32) so failures reproduce.
function rng(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const PAYLOADS = [
  'javascript:alert(1)', 'JaVaScRiPt:alert(document.domain)', 'data:text/html,<script>alert(1)</script>', 'file:///etc/passwd', 'https://user:pw@evil.example/',
  'https://example.com/ok', 'http://127.0.0.1:9877/v1/terminals', 'vbscript:x', '//evil.example', 'https://exa mple.com', '<img src=x onerror=alert(1)>',
  '<script>alert(1)</script>', '"><a href="javascript:alert(1)">x</a>', 'aGVsbG8gd29ybGQ=', '?', 'c;aGVsbG8=', 'p;Pw==', `${String.fromCodePoint(0x202e)}evil${String.fromCodePoint(0x202c)}`, 'x'.repeat(5000),
  'https://example.com/\u0000hidden', 'blob:https://example.com/uuid', 'ftp://example.com', 'mailto:a@b.c', 'https://example.com/a(b)c',
];

function pick<T>(r: () => number, list: readonly T[]): T {
  return list[Math.floor(r() * list.length)]!;
}

function sequence(r: () => number): string {
  const st = pick(r, ['\x07', '\x1b\\', '\x9c', '']);
  const payload = pick(r, PAYLOADS);
  switch (Math.floor(r() * 12)) {
    case 0:
      return `\x1b]52;${pick(r, ['c', 'p', 's', '0', ''])};${pick(r, ['aGVsbG8=', '?', payload])}${st}`;
    case 1:
      return `\x1b]8;${pick(r, ['', 'id=1', 'id=x:y'])};${payload}${st}${pick(r, ['click', payload, '\x1b[31mred'])}\x1b]8;;${st}`;
    case 2:
      return `\x1b]${pick(r, ['0', '1', '2'])};${payload}${st}`;
    case 3:
      return `\x1b]${pick(r, ['7', '9', '777', '1337', '133', '4', '10', '11', '12', '104', '22', '50', '5113'])};${payload}${st}`;
    case 4:
      // Queries that a naive terminal answers by injecting input (DA, DSR, title report, window ops, DECRQM, XTGETTCAP).
      return pick(r, ['\x1b[c', '\x1b[>c', '\x1b[5n', '\x1b[6n', '\x1b[21t', '\x1b[20t', '\x1b[14t', '\x1b[18t', '\x1b[?2004$p', '\x1bP+q544e\x1b\\', '\x1bP$qm\x1b\\', '\x1b[?u', '\x1b]10;?\x07', '\x1b]11;?\x1b\\']);
    case 5:
      return `${pick(r, ['\x1bP', '\x1b_', '\x1b^', '\x1bX', '\x90', '\x9f', '\x9e'])}${pick(r, ['q#0;2;0;0;0#0~~', 'Gf=100,a=T;AAAA', 'tmux;\x1b\x1b]52;c;aGk=\x07', payload])}${st}`;
    case 6: {
      const params = Array.from({ length: Math.floor(r() * 4) }, () => Math.floor(r() * 3000)).join(';');
      return `\x1b[${pick(r, ['', '?', '>', '='])}${params}${pick(r, ['m', 'H', 'J', 'K', 'h', 'l', 'r', 't', 'S', 'T', '@', 'P', 'X', 'q', 'n', 'c', 'u', 's'])}`;
    }
    case 7:
      return String.fromCharCode(...Array.from({ length: 1 + Math.floor(r() * 6) }, () => Math.floor(r() * 0x20)));
    case 8:
      return pick(r, ['<script>alert(1)</script>', '<img src=x onerror=alert(1)>', '<a href="https://evil.example">x</a>', '&lt;b&gt;', '<style>*{}</style>', '<iframe src=//evil>']);
    case 9:
      return pick(r, ['\x1b[?1049h', '\x1b[?1049l', '\x1b[?1000h\x1b[?1006h', '\x1b[?1003h', '\x1b[?2004h', '\x1b[?25l', '\x1b[?25h', '\x1bc', '\x1b[!p', '\x1b[?2026h', '\x1b[?2026l']);
    case 10:
      return `\x1b[38;2;${Math.floor(r() * 300)};${Math.floor(r() * 300)};${Math.floor(r() * 300)}m${pick(r, ['text', '中文', `e${String.fromCodePoint(0x301)}`, '👍🏽', payload.slice(0, 40)])}\x1b[0m`;
    default:
      return `${pick(r, ['plain ', 'see https://example.com/x. ', 'visit http://localhost:3000/path) ', '\r\n', '\t'])}`;
  }
}

/** Mix in invalid UTF-8 and C1 bytes at the byte level. */
function toBytes(text: string, r: () => number): Uint8Array {
  const bytes = Array.from(new TextEncoder().encode(text));
  if (r() < 0.1) bytes.splice(Math.floor(r() * (bytes.length + 1)), 0, pick(r, [0xff, 0xfe, 0xc0, 0x80, 0x9b, 0x9d, 0xed, 0xa0]));
  return Uint8Array.from(bytes);
}

const ALLOWED_TAGS = new Set(['DIV', 'SPAN']);
const ALLOWED_STYLE_PROPS = new Set(['color', 'background-color']);

function assertInertDom(root: HTMLElement) {
  for (const el of root.querySelectorAll('*')) {
    expect(ALLOWED_TAGS.has(el.tagName), `unexpected <${el.tagName}>`).toBe(true);
    for (const attr of el.attributes) expect(['class', 'style'], `attribute ${attr.name}`).toContain(attr.name);
    const style = (el as HTMLElement).style;
    for (let i = 0; i < style.length; i++) expect(ALLOWED_STYLE_PROPS.has(style.item(i)), `style ${style.item(i)}`).toBe(true);
    expect((el as HTMLElement).style.cssText).not.toMatch(/url\(|expression|@import/i);
  }
}

type Spy = ReturnType<typeof vi.fn>;
let spies: Record<"writeText" | "write" | "readText" | "open" | "createObjectURL" | "pushState" | "replaceState" | "anchorClick", Spy>;
let startHref: string;

beforeEach(() => {
  spies = {
    writeText: vi.fn(async () => undefined),
    write: vi.fn(async () => undefined),
    readText: vi.fn(async () => ''),
    open: vi.fn(() => null),
    createObjectURL: vi.fn(() => 'blob:x'),
    pushState: vi.fn(),
    replaceState: vi.fn(),
    anchorClick: vi.fn(),
  };
  vi.stubGlobal('navigator', Object.assign(Object.create(navigator), { clipboard: { writeText: spies.writeText, write: spies.write, readText: spies.readText } }));
  vi.spyOn(window, 'open').mockImplementation(spies.open as unknown as typeof window.open);
  URL.createObjectURL = spies.createObjectURL as unknown as typeof URL.createObjectURL;
  vi.spyOn(history, 'pushState').mockImplementation(spies.pushState as unknown as History["pushState"]);
  vi.spyOn(history, 'replaceState').mockImplementation(spies.replaceState as unknown as History["replaceState"]);
  vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(spies.anchorClick as unknown as () => void);
  startHref = window.location.href;
});

afterEach(() => {
  cleanup();
  document.body.replaceChildren();
});

function assertNoSideEffects() {
  for (const [name, spy] of Object.entries(spies)) expect(spy, name).not.toHaveBeenCalled();
  expect(window.location.href).toBe(startHref);
  expect(document.querySelector('script, iframe, object, embed, a[href^="javascript"]')).toBeNull();
}

describe('untrusted terminal output', () => {
  it('fuzz: 10 000 escape sequences cause no clipboard, navigation, download, input or markup', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    const container = document.createElement('div');
    document.body.appendChild(container);
    const titles: string[] = [];
    const links: DetectedLink[][] = [];
    const session = new TerminalSession(conn.asConnection(), container, { coreFactory: nodeCoreFactory, scrollbackLines: 500, theme: 'dark', schedule: syncSchedule }, {
      onTitle: t => titles.push(t),
      onLinks: l => links.push(l),
    });
    await session.start();
    conn.snapshots.emit({ cols: 80, rows: 24, screen: new Uint8Array(), hasOutput: false });
    const r = rng(0x0159);
    for (let batch = 0; batch < 100; batch++) {
      let text = '';
      for (let i = 0; i < 100; i++) text += sequence(r);
      const bytes = toBytes(text, r);
      // Split each batch at a random byte to exercise sequences spanning frames.
      const cut = Math.floor(r() * bytes.length);
      conn.outputs.emit(bytes.subarray(0, cut));
      conn.outputs.emit(bytes.subarray(cut));
      if (batch % 10 === 0) session.scrollBy(Math.floor(r() * 40) - 20);
      session.paint();
      assertInertDom(container);
    }
    session.dispose();
    assertNoSideEffects();
    expect(conn.inputs, 'terminal output must never produce PTY input').toHaveLength(0);
    for (const title of titles) {
      expect([...title].length).toBeLessThanOrEqual(MAX_TITLE_CHARS);
      for (const ch of title) expect(isUnsafeCodePoint(ch.codePointAt(0)!), `title char U+${ch.codePointAt(0)!.toString(16)}`).toBe(false);
    }
    for (const list of links) {
      for (const link of list) {
        expect(vetLink(link.url)).toBe(link.url);
        expect(link.url).toMatch(/^https?:\/\//);
      }
    }
    expect(titles.length).toBeGreaterThan(0);
    expect(links.flat().some(l => l.source === 'osc8')).toBe(true);
  });

  it('fuzz through the full view: no side effects and no anchors outside a link list', async () => {
    const View = createTerminalView(nodeCoreFactory);
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    const { container } = render(
      <View connection={conn.asConnection()} title="agent" focused control={{ holder: 'me', inputAllowed: true }} onRequestControl={() => undefined} onLinkDetected={() => undefined} onTitleChange={() => undefined} scrollbackLines={1000} touch reducedMotion theme="light" />,
    );
    await waitFor(() => expect(container.textContent).not.toContain('Loading terminal'));
    const r = rng(42);
    act(() => {
      conn.snapshots.emit({ cols: 60, rows: 12, screen: new Uint8Array(), hasOutput: false });
      for (let i = 0; i < 1000; i++) conn.outputs.emit(toBytes(sequence(r), r));
    });
    await new Promise(resolve => setTimeout(resolve, 50));
    expect(container.querySelector('a')).toBeNull();
    assertInertDom(container.querySelector('[role="log"]')!);
    assertNoSideEffects();
    expect(conn.inputs).toHaveLength(0);
  });

  it('OSC 52 shows a one-time notice and never touches the clipboard', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    const notices: string[] = [];
    const session = new TerminalSession(conn.asConnection(), document.createElement('div'), { coreFactory: nodeCoreFactory, scrollbackLines: 10, theme: 'dark', schedule: syncSchedule }, { onNotice: n => notices.push(n) });
    await session.start();
    conn.outputs.emit(new TextEncoder().encode('\x1b]52;c;aGVsbG8=\x07\x1b]52;c;d29ybGQ=\x07'));
    expect(notices).toEqual(['The agent asked to set your clipboard; awman blocked it. Select text and use Copy instead.']);
    session.dispose();
    assertNoSideEffects();
  });
});

describe('sanitisers', () => {
  it('titles: controls, bidi overrides and zero-width removed, whitespace collapsed, bounded', () => {
    expect(sanitizeTitle('a\x07b\x1b[31mc')).toBe('ab[31mc');
    expect(sanitizeTitle(`x${String.fromCodePoint(0x202e)}y${String.fromCodePoint(0x200b)}z${String.fromCodePoint(0x2066)}`)).toBe('xyz');
    expect(sanitizeTitle('  a \n\t b  ')).toBe('a b');
    expect([...sanitizeTitle('é'.repeat(500))].length).toBe(MAX_TITLE_CHARS);
    expect(sanitizeTitle(null)).toBe('');
    expect(sanitizeTitle('<b>bold</b>')).toBe('<b>bold</b>'); // rendered as text by React, never markup
  });
  it('links: only absolute http(s) without credentials or hidden characters', () => {
    expect(vetLink('https://example.com/a?b=c#d')).toBe('https://example.com/a?b=c#d');
    expect(vetLink('HTTP://Example.COM')).toBe('http://example.com/');
    for (const bad of ['javascript:alert(1)', 'data:text/html,x', 'file:///etc/passwd', 'https://u:p@example.com', '//example.com', 'ftp://x', 'mailto:a@b', 'https://exa mple.com', `https://example.com/${String.fromCodePoint(0x202e)}`, 'https://example.com/\x00', `https://e.com/${'a'.repeat(3000)}`, '', null]) {
      expect(vetLink(bad as string | null), String(bad)).toBeNull();
    }
  });
  it('plain-text URL detection trims punctuation and keeps balanced brackets', () => {
    expect(findUrls('see https://example.com/x. and (https://example.com/y) or https://en.wikipedia.org/wiki/Foo_(bar), ok')).toEqual([
      'https://example.com/x',
      'https://example.com/y',
      'https://en.wikipedia.org/wiki/Foo_(bar)',
    ]);
    expect(findUrls('javascript:alert(1) https://user:pw@x.example/')).toEqual([]);
  });
});
