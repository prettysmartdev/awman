// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { ConfigEditor } from '@/components/ConfigEditor';
import { request } from './helpers';
afterEach(cleanup);
function show() { const answer = vi.fn(); const r = request('config_show'); render(<ConfigEditor request={r} answer={answer} disabled={false} />); return { answer, r }; }
it('shows all config scopes, rejected values and saves through ConfigEdit', () => { const h = show(); expect(screen.getByRole('alert').textContent).toContain('unknown agent'); fireEvent.click(screen.getByRole('button', { name: 'Edit' })); fireEvent.change(screen.getByLabelText('Value'), { target: { value: 'codex' } }); fireEvent.click(screen.getByRole('button', { name: 'Save' })); expect(h.answer).toHaveBeenCalledWith({ kind: 'config_edit', value: { field: 'agent', value: 'codex', global: true } }); });
it('repo edits use the chosen scope and clear means an empty value', () => { const h = show(); fireEvent.click(screen.getByLabelText('Repository')); fireEvent.click(screen.getByRole('button', { name: 'Edit' })); fireEvent.click(screen.getByRole('button', { name: 'Clear' })); fireEvent.click(screen.getByRole('button', { name: 'Save' })); expect(h.answer).toHaveBeenCalledWith({ kind: 'config_edit', value: { field: 'agent', value: '', global: false } }); });
it('Escape cancels editing locally and then closes through Dismissed', () => { const h = show(); fireEvent.click(screen.getByRole('button', { name: 'Edit' })); fireEvent.keyDown(document, { key: 'Escape' }); expect(h.answer).not.toHaveBeenCalled(); fireEvent.keyDown(document, { key: 'Escape' }); expect(h.answer).toHaveBeenCalledWith({ kind: 'dismissed' }); });
it('header Add creates a new member field, respecting reserved Ctrl+N', () => { const h = show(); fireEvent.keyDown(document, { key: 'ArrowDown' }); fireEvent.keyDown(document, { key: 'n', ctrlKey: true }); expect(screen.queryByLabelText('Value')).toBeNull(); fireEvent.keyDown(document, { key: 'n', altKey: true }); fireEvent.change(screen.getByLabelText('Field'), { target: { value: 'env.NEW' } }); fireEvent.change(screen.getByLabelText('Value'), { target: { value: 'value' } }); fireEvent.click(screen.getByRole('button', { name: 'Save' })); expect(h.answer).toHaveBeenCalledWith({ kind: 'config_edit', value: { field: 'env.NEW', value: 'value', global: true } }); });
it('read-only fields have no edit action', () => { const r = request('config_show'); r.rows[0]!.read_only = true; render(<ConfigEditor request={r} answer={vi.fn()} disabled={false} />); expect((screen.getByRole('button', { name: 'Edit' }) as HTMLButtonElement).disabled).toBe(true); });

it('removes an entry explicitly using the existing empty-value ConfigEdit', () => {
  const h = show(); fireEvent.click(screen.getByLabelText('Repository')); fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
  fireEvent.click(screen.getByRole('button', { name: 'Remove entry' }));
  expect(h.answer).toHaveBeenCalledWith({ kind: 'config_edit', value: { field: 'agent', value: '', global: false } });
});
it('keeps context visible and rejects editing and answers when disabled', () => {
  const answer = vi.fn(); render(<ConfigEditor request={request('config_show')} answer={answer} disabled />);
  fireEvent.click(screen.getAllByRole('button', { pressed: true })[0]!); fireEvent.keyDown(document, { key: 'Enter' }); fireEvent.keyDown(document, { key: 'Escape' });
  expect(screen.queryByLabelText('Value')).toBeNull(); expect(answer).not.toHaveBeenCalled();
});
