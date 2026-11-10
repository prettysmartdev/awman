// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { Composer, composerPlaceholder } from '@/components/Composer';
import { harness } from './helpers';
afterEach(cleanup);
async function show(running = false) { const h = harness(); const tab = h.s.tabs[0]!; if (running) tab.execution_phase = { phase: 'running', command: 'ready' }; const quit = vi.fn(); render(<Composer client={h.client} connection={h.connection} tab={tab} readOnly={false} entries={[]} quit={quit} copy={vi.fn()} />); await screen.findByRole('button', { name: /readyready help|ready ready help/ }); const input = screen.getByRole('textbox', { name: 'Command' }) as HTMLTextAreaElement; input.focus(); return { ...h, input, quit }; }
it('sends raw commands once and waits for engine confirmation', async () => { const h = await show(); fireEvent.change(h.input, { target: { value: 'ready --unknown' } }); fireEvent.keyDown(h.input, { key: 'Enter' }); expect(h.send).toHaveBeenCalledWith({ type: 'submit', tab_id: h.s.tabs[0]!.tab_id, input: { source: 'command_box', raw: 'ready --unknown' }, pty_size: { cols: 100, rows: 30 } }); expect(h.input.readOnly).toBe(true); act(() => h.emit({ type: 'command_finished', tab_id: h.s.tabs[0]!.tab_id, result: { exit_code: 1, outcome: null, error: { code: 'parse_error', message: 'unknown flag' } } })); expect(h.input.value).toBe('ready --unknown'); expect(screen.getByRole('alert').textContent).toContain('unknown flag'); });
it('completes next/previous catalogue suggestions and flag help', async () => { const h = await show(); fireEvent.keyDown(h.input, { key: 'Tab' }); expect(h.input.value).toBe('ready'); fireEvent.keyDown(h.input, { key: 'Tab' }); expect(h.input.value).toBe('review'); fireEvent.keyDown(h.input, { key: 'Tab', shiftKey: true }); expect(h.input.value).toBe('ready'); fireEvent.change(h.input, { target: { value: 'ready --' } }); expect(screen.getByText(/Watch updates/)).toBeTruthy(); fireEvent.keyDown(h.input, { key: 'Tab' }); expect(h.input.value).toBe('ready --watch'); });
it('adds newlines at the native cursor and q quits only with empty input', async () => { const h = await show(); fireEvent.change(h.input, { target: { value: 'abc' } }); h.input.setSelectionRange(1, 1); fireEvent.keyDown(h.input, { key: 'Enter', shiftKey: true }); expect(h.input.value).toBe('a\nbc'); fireEvent.change(h.input, { target: { value: '' } }); fireEvent.keyDown(h.input, { key: 'q' }); expect(h.quit).toHaveBeenCalledTimes(1); });
it('locks the composer during execution', async () => { const h = await show(true); expect(h.input.readOnly).toBe(true); expect((screen.getByRole('button', { name: 'Run' }) as HTMLButtonElement).disabled).toBe(true); fireEvent.keyDown(h.input, { key: 'Enter' }); expect(h.send).not.toHaveBeenCalled(); });
it('shows the TUI "q to quit" ghost hint only while empty and idle/done (review #11, cmd.ghost_text)', async () => {
  const h = await show();
  expect(h.input.placeholder).toMatch(/^q to quit/);
  cleanup();
  const running = await show(true);
  expect(running.input.placeholder).not.toMatch(/q to quit/);
  expect(composerPlaceholder('done', false)).toMatch(/^q to quit/);
  expect(composerPlaceholder('error', false)).not.toMatch(/q to quit/);
  expect(composerPlaceholder('idle', true)).not.toMatch(/q to quit/);
});
