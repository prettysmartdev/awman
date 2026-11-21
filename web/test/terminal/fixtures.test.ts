// @vitest-environment jsdom
// Replays every recorded TerminalHub fixture (src/terminal/fixtures, generated
// with the hub's own vt100-ctt parser) through the real adapter: wterm's
// Ghostty core + the DOM renderer. Two paths per fixture:
//   stream   — initial snapshot, then every output/resize frame in order;
//   snapshot — only the hub's final snapshot (lagged/reconnect resync).
// Expected screens come from the hub mirror (`expect`, `expect_from_snapshot`).
import { afterEach, describe, expect, it } from 'vitest';
import { TerminalSession } from '@/terminal/adapter/session';
import { FakeConnection, loadFixtures, nodeCoreFactory, numberedLinesFrames, syncSchedule, type Expectation, type Fixture, type FixtureFrame } from './helpers';

const sessions: TerminalSession[] = [];
afterEach(() => {
  for (const s of sessions.splice(0)) s.dispose();
  document.body.replaceChildren();
});

async function replay(frames: FixtureFrame[], scrollbackLines = 10_000) {
  const connection = new FakeConnection();
  const container = document.createElement('div');
  document.body.appendChild(container);
  const titles: string[] = [];
  const session = new TerminalSession(connection.asConnection(), container, { coreFactory: nodeCoreFactory, scrollbackLines, theme: 'dark', schedule: syncSchedule }, { onTitle: t => titles.push(t) });
  sessions.push(session);
  await session.start();
  for (const frame of frames) connection.frame(frame);
  session.paint();
  return { session, container, titles };
}

const segmenter = new Intl.Segmenter(undefined, { granularity: 'grapheme' });
const graphemes = (text: string) => [...segmenter.segment(text)].length;

function domLines(container: HTMLElement): string[] {
  return [...container.querySelectorAll('.term-row')].map(row => (row.textContent ?? '').trimEnd());
}

function domCursor(container: HTMLElement): { row: number; col: number } | null {
  const rows = [...container.querySelectorAll('.term-row')];
  for (let r = 0; r < rows.length; r++) {
    let col = 0;
    for (const span of rows[r]!.querySelectorAll('span')) {
      if (span.classList.contains('tc')) return { row: r, col };
      col += span.classList.contains('tw') ? 2 : graphemes(span.textContent ?? '');
    }
  }
  return null;
}

function check(fx: Fixture, label: string, result: Awaited<ReturnType<typeof replay>>, want: Expectation) {
  expect(domLines(result.container), `${fx.name} ${label} text`).toEqual(want.lines);
  const cursor = domCursor(result.container);
  if (want.cursor.visible) expect(cursor, `${fx.name} ${label} cursor`).toEqual({ row: want.cursor.row, col: want.cursor.col });
  else expect(cursor, `${fx.name} ${label} hidden cursor`).toBeNull();
  const modes = result.session.modes();
  expect(modes.altScreen, `${fx.name} ${label} alt screen`).toBe(want.alt_screen);
  expect(modes.appCursor, `${fx.name} ${label} DECCKM`).toBe(want.app_cursor);
  expect(modes.bracketedPaste, `${fx.name} ${label} 2004`).toBe(want.bracketed_paste);
  if (want.mouse_mode !== undefined) expect(modes.mouse, `${fx.name} ${label} mouse`).toBe(want.mouse_mode);
  if (want.mouse_sgr !== undefined) expect(modes.sgr, `${fx.name} ${label} sgr`).toBe(want.mouse_sgr);
}

const fixtures = loadFixtures();
// Grapheme-cluster width is a known parser divergence (see fixtures/README.md).
const exact = fixtures.filter(fx => fx.name !== 'emoji_sequences');

describe('agent-TUI fixtures (wterm Ghostty core, DOM renderer)', () => {
  it('has the recorded fixture set', () => {
    expect(fixtures.map(f => f.name)).toEqual(
      expect.arrayContaining(['utf8_split', 'wide_graphemes', 'emoji_sequences', 'alt_screen_redraw', 'alt_screen_exit', 'colours', 'cursor_modes', 'mouse_sgr', 'mouse_any_x10enc', 'bracketed_paste', 'resize_storm', 'scrollback_10k', 'untrusted_osc']),
    );
  });

  for (const fx of exact) {
    it(`${fx.name}: stream replay matches the hub screen`, async () => {
      const frames = fx.generated_output ? [...fx.frames, ...numberedLinesFrames()] : fx.frames;
      check(fx, 'stream', await replay(frames), fx.expect);
    });
    it(`${fx.name}: snapshot-only resync matches the hub snapshot`, async () => {
      check(fx, 'snapshot', await replay([fx.final_snapshot]), fx.expect_from_snapshot);
    });
  }

  it('emoji sequences keep every grapheme cluster intact', async () => {
    const fx = fixtures.find(f => f.name === 'emoji_sequences')!;
    const { container } = await replay(fx.frames);
    const text = domLines(container).join('\n');
    for (const cluster of ['👍🏽', '🇯🇵', '👨‍👩‍👧', 'ok']) expect(text).toContain(cluster);
  });

  it('a mid-stream snapshot resets the screen (reconnect / lagged)', async () => {
    const fx = fixtures.find(f => f.name === 'alt_screen_redraw')!;
    const { container, session } = await replay([...fx.frames, fixtures.find(f => f.name === 'colours')!.final_snapshot]);
    expect(domLines(container)).toEqual(fixtures.find(f => f.name === 'colours')!.expect_from_snapshot.lines);
    expect(session.gridSize()).toEqual({ cols: 40, rows: 4 });
  });

  it('colours: palette, 256 and true colour reach the DOM through CSSOM, attributes as classes', async () => {
    const fx = fixtures.find(f => f.name === 'colours')!;
    const { container } = await replay(fx.frames);
    const spans = [...container.querySelectorAll('span')];
    const find = (text: string) => spans.find(s => s.textContent === text)!;
    expect(find('red').style.color).toBe('rgb(229, 83, 75)'); // dark theme palette[1]
    expect(find('bold green').classList.contains('tb')).toBe(true);
    expect(find('ital-ul').className).toContain('ti');
    expect(find('ital-ul').className).toContain('tu');
    expect(find('256red').style.color).toBe('rgb(255, 0, 0)');
    expect(find(' on blue').style.backgroundColor).toBe('rgb(0, 0, 255)');
    expect(find('tc').style.color).toBe('rgb(1, 2, 3)');
    expect(find('inv').style.backgroundColor).not.toBe('');
    expect(find('strike').classList.contains('ts')).toBe(true);
    expect(find('dim').classList.contains('td')).toBe(true);
  });

  it('ignores snapshot/resize frames with malformed sizes', async () => {
    const fx = fixtures.find(f => f.name === 'colours')!;
    const { session, container } = await replay([
      ...fx.frames,
      { type: 'resize', cols: 100_000, rows: 5 },
      { type: 'resize', cols: 0, rows: 5 },
      { type: 'snapshot', cols: 1.5, rows: 2, screen: '' },
    ]);
    expect(session.gridSize()).toEqual({ cols: 40, rows: 4 });
    expect(domLines(container)).toEqual(fx.expect.lines);
  });

  it('scrollback is bounded by scrollbackLines', async () => {
    const fx = fixtures.find(f => f.name === 'scrollback_10k')!;
    const small = await replay([...fx.frames, ...numberedLinesFrames()], 1000);
    expect(domLines(small.container)).toEqual(fx.expect.lines);
    // Scroll to the very top of history: the oldest retained line is ~1000 lines back, not line 0.
    small.session.scrollBy(1_000_000);
    small.session.paint();
    const top = domLines(small.container)[0]!;
    const n = Number(/line (\d+)/.exec(top)?.[1]);
    expect(n).toBeGreaterThan(8_900);
    expect(n).toBeLessThan(9_100);
  });
});
