// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { WorkspacePicker } from '@/components/WorkspacePicker';
import { FrontendConnection } from '@/lib/client/frontendSocket';
import { writeResume } from '@/lib/state/session';
import { harness } from './helpers';
vi.mock('@/lib/client/frontendSocket', () => ({ FrontendConnection: vi.fn(function (this: { marker: boolean }) { this.marker = true; }) }));
afterEach(() => { cleanup(); sessionStorage.clear(); vi.clearAllMocks(); });
it('preselects the default workspace, creates only on gesture, and sends primary Hello options', async () => { const h = harness(); const open = vi.fn(); render(<WorkspacePicker client={h.client} onOpen={open} />); await screen.findByRole('option', { name: /Project/ }); expect((screen.getByLabelText('Workspace') as HTMLSelectElement).value).toBe('lc_default'); expect(h.api.createInstance).not.toHaveBeenCalled(); fireEvent.click(screen.getByRole('button', { name: 'Open workspace' })); await waitFor(() => expect(open).toHaveBeenCalled()); expect(FrontendConnection).toHaveBeenCalledWith(h.client, 'ins_new', { role: 'primary', launchContext: 'lc_default', termSize: { cols: 100, rows: 30 } }); });
it('shows the registration recipe without guessing a cwd', async () => { const h = harness(); vi.spyOn(h.client, 'launchContexts').mockResolvedValue({ items: [], default_id: null, next_cursor: null }); render(<WorkspacePicker client={h.client} onOpen={vi.fn()} />); await screen.findByText('awman daemon api launch_contexts.create default=true'); expect((screen.getByRole('button', { name: 'Open workspace' }) as HTMLButtonElement).disabled).toBe(true); });
it('attaches to existing browser instances as secondary without creating another', async () => { const h = harness(); vi.spyOn(h.client, 'listInstances').mockResolvedValue([{ id: 'ins_existing', primary_frontend_kind: 'web', annotations: { label: 'Existing' }, tab_count: 2, running_jobs: 1, created_at: '' }]); const open = vi.fn(); render(<WorkspacePicker client={h.client} onOpen={open} />); fireEvent.click(await screen.findByRole('button', { name: 'Attach read only' })); await waitFor(() => expect(open).toHaveBeenCalled()); expect(h.api.createInstance).not.toHaveBeenCalled(); expect(FrontendConnection).toHaveBeenCalledWith(h.client, 'ins_existing', { role: 'secondary', termSize: { cols: 100, rows: 30 } }); });
it('resumes from ids only after pairing, with no command replay', async () => { const h = harness(); h.connection.onClose = () => () => undefined; vi.mocked(FrontendConnection).mockImplementationOnce(function () { return h.connection; }); writeResume({ instanceId: 'ins_resume', frontendId: 'fe_resume', tabId: 'tab_resume', launchContext: 'lc_default' }); const open = vi.fn(); render(<WorkspacePicker client={h.client} onOpen={open} />); await waitFor(() => expect(open).toHaveBeenCalled()); expect(FrontendConnection).toHaveBeenCalledWith(h.client, 'ins_resume', { role: 'primary', resumeFrontendId: 'fe_resume', termSize: { cols: 100, rows: 30 } }); expect(h.api.createInstance).not.toHaveBeenCalled(); });
it('keeps engine configuration errors visible on failed creation', async () => { const h = harness(); vi.spyOn(h.client, 'createInstance').mockRejectedValue(new Error('config_invalid: invalid agent')); render(<WorkspacePicker client={h.client} onOpen={vi.fn()} />); await screen.findByRole('option', { name: /Project/ }); fireEvent.click(screen.getByRole('button', { name: 'Open workspace' })); await screen.findByText('config_invalid: invalid agent'); });
it('renders the full selected path outside the native select and retains stale environment information', async () => {
  const h = harness(); h.contexts.items[0]!.root = '/projects/' + 'long-path/'.repeat(20); h.contexts.items[0]!.env_state = 'stale';
  render(<WorkspacePicker client={h.client} onOpen={vi.fn()} />);
  const path = await screen.findByText(h.contexts.items[0]!.root);
  expect(path.tagName).toBe('CODE'); expect(screen.getByText(/Environment stale/)).toBeTruthy();
  expect(screen.getByRole('combobox').getAttribute('aria-describedby')).toBe('workspace-detail');
});
it('rechecks after a registration or loading failure without creating a workspace', async () => {
  const h = harness(); h.api.launchContexts.mockRejectedValueOnce(new Error('offline: try again'));
  render(<WorkspacePicker client={h.client} onOpen={vi.fn()} />);
  await screen.findByText('offline: try again'); fireEvent.click(screen.getByRole('button', { name: 'Recheck' }));
  await screen.findByRole('option', { name: /Project/ }); expect(h.api.createInstance).not.toHaveBeenCalled();
});
it('ignores a second open gesture while instance creation is pending', async () => {
  const h = harness(); let finish!: (value: { id: string }) => void;
  h.api.createInstance.mockImplementation(() => new Promise(resolve => { finish = resolve; }));
  const open = vi.fn(); render(<WorkspacePicker client={h.client} onOpen={open} />);
  await screen.findByRole('option', { name: /Project/ });
  const button = screen.getByRole('button', { name: 'Open workspace' }); fireEvent.click(button); fireEvent.click(button);
  finish({ id: 'ins_new' }); await waitFor(() => expect(open).toHaveBeenCalledOnce()); expect(h.api.createInstance).toHaveBeenCalledOnce();
});
it('does not open a default id missing from the returned workspace list', async () => {
  const h = harness(); vi.spyOn(h.client, 'launchContexts').mockResolvedValue({ items: [], default_id: 'lc_missing', next_cursor: null });
  render(<WorkspacePicker client={h.client} onOpen={vi.fn()} />);
  await screen.findByRole('button', { name: 'Copy workspace registration command' });
  fireEvent.click(screen.getByRole('button', { name: 'Open workspace' })); expect(h.api.createInstance).not.toHaveBeenCalled();
});
