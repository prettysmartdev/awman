// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { createRef, useSyncExternalStore } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createTerminalView } from '@/terminal/TerminalView';
import type { DetectedLink, TerminalViewHandle, TerminalViewProps } from '@/terminal/contract';
import { FakeConnection, fixture, nodeCoreFactory } from './helpers';

const TerminalView = createTerminalView(nodeCoreFactory);
afterEach(() => cleanup());

function Harness(props: { conn: FakeConnection; handle?: React.Ref<TerminalViewHandle>; overrides?: Partial<TerminalViewProps> }) {
  const control = useSyncExternalStore(
    cb => props.conn.control.subscribe(() => cb()),
    () => props.conn.control.get(),
  );
  return (
    <TerminalView
      ref={props.handle}
      connection={props.conn.asConnection()}
      title="claude"
      focused
      control={control}
      onRequestControl={() => void props.conn.takeControl()}
      onLinkDetected={() => undefined}
      scrollbackLines={10_000}
      touch={false}
      reducedMotion={false}
      theme="dark"
      {...props.overrides}
    />
  );
}

const rows = (container: HTMLElement) => [...container.querySelectorAll('.term-row')].map(r => (r.textContent ?? '').trimEnd());
const input = () => screen.getByLabelText('Type into claude') as HTMLTextAreaElement;

async function mount(conn: FakeConnection, overrides?: Partial<TerminalViewProps>, handle?: React.Ref<TerminalViewHandle>) {
  const view = render(<Harness conn={conn} overrides={overrides} handle={handle} />);
  await waitFor(() => expect(screen.queryByText('Loading terminal…')).toBeNull());
  return view;
}

function feed(conn: FakeConnection, name: string) {
  act(() => {
    for (const frame of fixture(name).frames) conn.frame(frame);
  });
}

describe('TerminalView', () => {
  it('renders hub frames and sends typed keys without local echo', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    const { container } = await mount(conn);
    feed(conn, 'bracketed_paste');
    await waitFor(() => expect(rows(container)[0]).toBe('>'));
    fireEvent.keyDown(input(), { key: 'l' });
    fireEvent.keyDown(input(), { key: 's' });
    fireEvent.keyDown(input(), { key: 'Enter' });
    fireEvent.keyDown(input(), { key: 'c', ctrlKey: true });
    expect(conn.sent()).toBe('ls\r\x03');
    // No echo: the screen only changes when the PTY output comes back.
    await new Promise(r => setTimeout(r, 40));
    expect(rows(container)[0]).toBe('>');
    act(() => conn.outputs.emit(new TextEncoder().encode('ls')));
    await waitFor(() => expect(rows(container)[0]).toBe('> ls'));
  });

  it('first keypress without control requests it and sends nothing', async () => {
    const conn = new FakeConnection({ holder: 'none', inputAllowed: true });
    await mount(conn);
    expect(screen.getByRole('button', { name: 'Take control' })).toBeTruthy();
    fireEvent.keyDown(input(), { key: 'x' });
    expect(conn.takeControlCalls).toBe(1);
    expect(conn.inputs).toHaveLength(0);
    await waitFor(() => expect(screen.getByText('You have control')).toBeTruthy());
    fireEvent.keyDown(input(), { key: 'x' });
    expect(conn.sent()).toBe('x');
  });

  it('read-only viewers get no take-control button and send nothing', async () => {
    const conn = new FakeConnection({ holder: 'none', inputAllowed: false });
    await mount(conn);
    expect(screen.getByText('Read-only')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Take control' })).toBeNull();
    fireEvent.keyDown(input(), { key: 'x' });
    expect(conn.takeControlCalls).toBe(0);
    expect(conn.inputs).toHaveLength(0);
  });

  it('shows who holds control', async () => {
    const conn = new FakeConnection({ holder: 'other', inputAllowed: true });
    await mount(conn);
    expect(screen.getByText('Someone else has control')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Take control' }));
    expect(conn.takeControlCalls).toBe(1);
  });

  it('leaves global shortcuts to the app (not sent, not default-prevented)', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    await mount(conn);
    for (const key of [{ key: 'g', ctrlKey: true }, { key: 'o', ctrlKey: true }, { key: 'y', ctrlKey: true }, { key: '\\', ctrlKey: true }]) {
      const event = new KeyboardEvent('keydown', { ...key, bubbles: true, cancelable: true });
      input().dispatchEvent(event);
      expect(event.defaultPrevented).toBe(false);
    }
    expect(conn.inputs).toHaveLength(0);
  });

  it('ignores keys while not focused', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    await mount(conn, { focused: false });
    fireEvent.keyDown(input(), { key: 'x' });
    expect(conn.inputs).toHaveLength(0);
  });

  it('uses application cursor keys once the agent enables DECCKM', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    await mount(conn);
    feed(conn, 'cursor_modes');
    fireEvent.keyDown(input(), { key: 'ArrowUp' });
    expect(conn.sent()).toBe('\x1bOA');
  });

  it('pastes with bracketed-paste markers when mode 2004 is on', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    await mount(conn);
    feed(conn, 'bracketed_paste');
    fireEvent.paste(input(), { clipboardData: { getData: () => 'echo hi\n' } });
    expect(conn.sent()).toBe('\x1b[200~echo hi\r\x1b[201~');
  });

  it('refuses a paste over 1 MiB with a visible message', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    await mount(conn);
    fireEvent.paste(input(), { clipboardData: { getData: () => 'x'.repeat(1024 * 1024 + 1) } });
    expect(conn.inputs).toHaveLength(0);
    expect(screen.getByText('Paste is larger than 1 MiB and was not sent.')).toBeTruthy();
  });

  it('resizes the PTY only while holding control', async () => {
    vi.spyOn(HTMLElement.prototype, 'clientWidth', 'get').mockReturnValue(800);
    vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockReturnValue(260);
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue({ width: 80, height: 13, top: 0, left: 0, right: 80, bottom: 13, x: 0, y: 0, toJSON: () => ({}) } as DOMRect);
    const conn = new FakeConnection({ holder: 'none', inputAllowed: true });
    const handle = createRef<TerminalViewHandle>();
    await mount(conn, undefined, handle);
    act(() => handle.current?.fit());
    expect(conn.resizeCalls).toEqual([]);
    await act(async () => {
      await conn.takeControl();
    });
    await waitFor(() => expect(conn.resizeCalls).toEqual([{ cols: 100, rows: 20 }]));
    // The local grid follows the hub's resize frame, not the request.
    act(() => conn.resizes.emit({ cols: 100, rows: 20 }));
    act(() => handle.current?.fit());
    expect(conn.resizeCalls).toHaveLength(1);
  });

  it('touch toolbar sends control keys and latches Ctrl', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    await mount(conn, { touch: true });
    fireEvent.click(screen.getByRole('button', { name: 'Escape' }));
    fireEvent.click(screen.getByRole('button', { name: 'Up arrow' }));
    fireEvent.click(screen.getByRole('button', { name: 'Control C (interrupt)' }));
    fireEvent.click(screen.getByRole('button', { name: 'Control (applies to the next key)' }));
    expect(screen.getByRole('button', { name: 'Control (applies to the next key)' }).getAttribute('aria-pressed')).toBe('true');
    const sink = input();
    sink.value = 'd';
    fireEvent.input(sink);
    expect(conn.sent()).toBe('\x1b\x1b[A\x03\x04');
    expect(sink.value).toBe('');
  });

  it('soft-keyboard / IME text arrives through input events', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    await mount(conn);
    const sink = input();
    sink.value = 'héllo';
    fireEvent.input(sink);
    fireEvent.compositionEnd(sink, { data: '日本' });
    expect(new TextDecoder().decode(Uint8Array.from(Buffer.from(conn.sent(), 'latin1')))).toBe('héllo日本');
  });

  it('reports sanitised titles and vetted links, shows the exit state', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    const titles: string[] = [];
    const links: DetectedLink[][] = [];
    const { container } = await mount(conn, { onTitleChange: t => titles.push(t), onLinkDetected: l => links.push(l) });
    feed(conn, 'untrusted_osc');
    await waitFor(() => {
      expect(titles.at(-1)).toBe('Claude <script>alert(1)</script>');
      expect(links.at(-1)).toEqual([{ url: 'https://example.com/docs', text: 'docs', source: 'osc8' }]);
    });
    expect(container.querySelector('a, script, img, iframe')).toBeNull();
    act(() => conn.exits.emit(3));
    expect(screen.getByText('The agent exited (code 3).')).toBeTruthy();
  });

  it('copies the selection only when asked (user gesture)', async () => {
    const writeText = vi.fn(async () => undefined);
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText, readText: vi.fn() } });
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    const handle = createRef<TerminalViewHandle>();
    const { container } = await mount(conn, undefined, handle);
    feed(conn, 'colours');
    await waitFor(() => expect(rows(container)[0]).toContain('red'));
    expect(writeText).not.toHaveBeenCalled();
    const row = container.querySelector('.term-row')!;
    const range = document.createRange();
    range.selectNodeContents(row);
    window.getSelection()!.removeAllRanges();
    window.getSelection()!.addRange(range);
    await act(async () => handle.current?.copySelection());
    expect(writeText).toHaveBeenCalledWith(expect.stringContaining('red bold green'));
  });

  it('scrolls history with Shift+PageUp and returns to live on typing', async () => {
    const conn = new FakeConnection({ holder: 'me', inputAllowed: true });
    const { container } = await mount(conn);
    act(() => {
      conn.frame(fixture('scrollback_10k').frames[0]!);
      let text = '';
      for (let i = 0; i < 100; i++) text += `row ${i}\r\n`;
      conn.outputs.emit(new TextEncoder().encode(text));
    });
    await waitFor(() => expect(rows(container)).toContain('row 99'));
    fireEvent.keyDown(input(), { key: 'PageUp', shiftKey: true });
    await waitFor(() => expect(rows(container)).not.toContain('row 99'));
    expect(screen.getByRole('button', { name: 'Back to live' })).toBeTruthy();
    expect(conn.inputs).toHaveLength(0);
    fireEvent.keyDown(input(), { key: 'q' });
    await waitFor(() => expect(rows(container)).toContain('row 99'));
  });
});
