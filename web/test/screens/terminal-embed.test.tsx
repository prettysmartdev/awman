// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { Subject } from '@/lib/client/observable';
import type { TerminalViewProps } from '@/terminal/contract';
import type { TerminalControl } from '@/lib/client/terminalSocket';
import { TerminalPanel } from '@/components/TerminalPanel';
import { harness } from './helpers';
const captured = vi.hoisted(() => ({ props: null as TerminalViewProps | null, close: vi.fn(), take: vi.fn(async () => undefined), output: null as (() => void) | null }));
vi.mock('@/terminal/contract', () => ({ DEFAULT_SCROLLBACK_LINES: 10_000, TerminalView: (props: TerminalViewProps) => { captured.props = props; return <button onClick={props.onRequestControl}>Take control</button>; } }));
vi.mock('@/lib/client/terminalSocket', () => ({ TerminalConnection: class {
  control = new Subject<TerminalControl>({ holder: 'none', inputAllowed: true }); state = new Subject('open');
  close = captured.close; takeControl = captured.take;
  onError() { return () => undefined; }
  onOutput(cb: () => void) { captured.output = cb; return () => { captured.output = null; }; }
} }));
afterEach(() => { cleanup(); vi.useRealTimers(); vi.clearAllMocks(); });
it('embeds through the terminal contract, requests control and detaches on unmount', () => { const h = harness(); const mounted = vi.fn(); const view = render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent · setup" focused={true} onMounted={mounted} />); expect(captured.props?.title).toBe('Agent · setup'); expect(captured.props?.scrollbackLines).toBe(10000); expect(mounted).toHaveBeenCalledTimes(1); fireEvent.click(screen.getByRole('button', { name: 'Take control' })); expect(captured.take).toHaveBeenCalled(); view.unmount(); expect(captured.close).toHaveBeenCalledTimes(1); });
it('shows working-silently time since the last output without inventing progress', () => { vi.useFakeTimers(); vi.setSystemTime(100_000); const h = harness(); render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" working />); act(() => vi.advanceTimersByTime(31_000)); expect(screen.getByText('Working silently · last output 31s ago')).toBeTruthy(); act(() => captured.output?.()); expect(screen.queryByText(/Working silently/)).toBeNull(); });
it('renders detected links only on user-visible controls, rejecting other schemes', () => { const h = harness(); render(<TerminalPanel client={h.client} terminalId="term_one" title="Agent" />); act(() => captured.props?.onLinkDetected([{ url: 'javascript:bad()', text: 'bad', source: 'osc8' }, { url: 'https://example.com/docs', text: 'Docs', source: 'text' }])); const link = screen.getByRole('link', { name: 'Docs', hidden: true }); expect(link.getAttribute('rel')).toBe('noopener noreferrer'); expect(document.querySelector('a[href^="javascript"]')).toBeNull(); });
