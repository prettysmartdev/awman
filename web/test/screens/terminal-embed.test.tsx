// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { Subject } from '@/lib/client/observable';
import type { TerminalViewProps } from '@/terminal/contract';
import type { TerminalControl } from '@/lib/client/terminalSocket';
import { TerminalPanel } from '@/components/TerminalPanel';
import { harness } from './helpers';
const captured = vi.hoisted(() => ({ props: null as TerminalViewProps | null, input: vi.fn(), control: null as Subject<TerminalControl> | null, close: vi.fn(), take: vi.fn(async () => undefined), output: null as (() => void) | null, error: null as ((e: { code: string; message: string }) => void) | null }));
vi.mock('@/terminal/contract', () => ({ DEFAULT_SCROLLBACK_LINES: 10_000, TerminalView: (props: TerminalViewProps) => { captured.props = props; return <section>{props.barStart}<button onClick={props.onRequestControl}>Take control</button>{props.barEnd}<div data-testid="overlay">{props.overlay}</div></section>; } }));
vi.mock('@/lib/client/terminalSocket', () => ({ TerminalConnection: class {
  control = new Subject<TerminalControl>({ holder: 'none', inputAllowed: true }); state = new Subject('open');
  constructor() { captured.control = this.control; }
  input = captured.input;
  close = captured.close; takeControl = captured.take;
  onError(cb: (e: { code: string; message: string }) => void) { captured.error = cb; return () => { captured.error = null; }; }
  onOutput(cb: () => void) { captured.output = cb; return () => { captured.output = null; }; }
} }));
afterEach(() => { cleanup(); vi.useRealTimers(); vi.clearAllMocks(); });
it('embeds through the terminal contract, requests control and detaches on unmount', () => { const h = harness(); const mounted = vi.fn(); const view = render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent · setup" focused={true} onMounted={mounted} />); expect(captured.props?.title).toBe('Agent · setup'); expect(captured.props?.scrollbackLines).toBe(10000); expect(mounted).toHaveBeenCalledTimes(1); fireEvent.click(screen.getByRole('button', { name: 'Take control' })); expect(captured.take).toHaveBeenCalled(); view.unmount(); expect(captured.close).toHaveBeenCalledTimes(1); });
it('shows working-silently time since the last output without inventing progress', () => { vi.useFakeTimers(); vi.setSystemTime(100_000); const h = harness(); render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" working />); act(() => vi.advanceTimersByTime(31_000)); expect(screen.getByText('Working silently · last output 31s ago')).toBeTruthy(); act(() => captured.output?.()); expect(screen.queryByText(/Working silently/)).toBeNull(); });
it('renders detected links only on user-visible controls, rejecting other schemes', () => { const h = harness(); render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" />); act(() => captured.props?.onLinkDetected([{ url: 'javascript:bad()', text: 'bad', source: 'osc8' }, { url: 'https://example.com/docs', text: 'Docs', source: 'text' }])); fireEvent.click(screen.getByRole('button', { name: 'Agent actions' })); fireEvent.click(screen.getByRole('menuitem', { name: /Links detected/ })); const link = screen.getByRole('link', { name: 'Docs', hidden: true }); expect(link.getAttribute('rel')).toBe('noopener noreferrer'); expect(document.querySelector('a[href^="javascript"]')).toBeNull(); });

it('offers all-width Ctrl+C with a live ownership gate', () => {
  const h = harness(); render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" />);
  fireEvent.click(screen.getByRole('button', { name: 'Agent actions' }));
  expect((screen.getByRole('menuitem', { name: /Send Ctrl\+C/ }) as HTMLButtonElement).disabled).toBe(true);
  act(() => captured.control?.set({ holder: 'me', inputAllowed: true }));
  fireEvent.click(screen.getByRole('menuitem', { name: 'Send Ctrl+C' })); expect(captured.input).toHaveBeenCalledWith(new Uint8Array([3]));
  act(() => captured.control?.set({ holder: 'none', inputAllowed: false }));
  fireEvent.click(screen.getByRole('button', { name: 'Agent actions' }));
  expect((screen.getByRole('menuitem', { name: /Paste/ }) as HTMLButtonElement).disabled).toBe(true);
  expect((screen.getByRole('menuitem', { name: 'Copy selection' }) as HTMLButtonElement).disabled).toBe(false);
});

it('lays status lines over the terminal, lets errors be dismissed and merges window actions into one menu', () => {
  const h = harness(); const hide = vi.fn();
  render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" lead={<span>Lead identity</span>} windowActions={[{ id: 'hide', label: 'Hide agent', group: 'Agent', matrix: [], run: hide }]} overlay={<p>Countdown banner</p>} />);
  expect(screen.getByText('Lead identity')).toBeTruthy();
  expect(screen.getByTestId('overlay').textContent).toContain('Countdown banner');
  expect(captured.props?.touch).toBe(false); // no coarse pointer: no key toolbar
  fireEvent.click(screen.getByRole('button', { name: 'Agent actions' }));
  fireEvent.click(screen.getByRole('menuitem', { name: 'Hide agent' })); expect(hide).toHaveBeenCalled();
  act(() => captured.error?.({ code: 'control_held', message: 'Someone else holds control.' }));
  const alert = screen.getByRole('alert'); expect(screen.getByTestId('overlay').contains(alert)).toBe(true);
  fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
  expect(screen.queryByRole('alert')).toBeNull();
});
it('keeps the terminal connection across a frontend-socket blip and only gates input', () => {
  const h = harness();
  const view = render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" viewOnly={false} readOnly={false} />);
  view.rerender(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" viewOnly={false} readOnly={true} />);
  expect(captured.close).not.toHaveBeenCalled();
  act(() => captured.control?.set({ holder: 'me', inputAllowed: true }));
  expect(captured.props?.control.inputAllowed).toBe(false);
  view.rerender(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" viewOnly={false} readOnly={false} />);
  expect(captured.close).not.toHaveBeenCalled();
  expect(captured.props?.control).toEqual({ holder: 'me', inputAllowed: true });
});
