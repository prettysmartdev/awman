'use client';
import { useEffect, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { SquadRun, SquadTask } from '@/lib/client/types';
import { useScreenShortcuts as useShortcuts } from './ui';
import { Modal, VirtualList, actionArgs, errorText } from './ui';
import { PromptButtons, usePrompt } from './Dialogs';
import { TerminalPanel } from './TerminalPanel';

type TaskAction = 'pause' | 'trigger' | 'cancel' | 'delete';
function TaskConfirm({ client, action, task, confirm, dismiss, busy }: { client: DaemonClient; action: TaskAction; task: SquadTask; confirm: () => void; dismiss: () => void; busy: boolean }) {
  const actionType = action;
  const { prompt, error } = usePrompt(client, action === 'delete' ? null : 'squad_task_confirm', { action, name: task.name });
  function choose(value: string) { if (value === 'dispatch' || value === 'yes') confirm(); else dismiss(); }
  useShortcuts(() => ({ context: 'dialog', opts: { dialog: 'yes_no' } }), action => { if (action === 'dialog.dismiss' || action === 'dialog.no') { dismiss(); return; } if (action === 'dialog.yes') { if (prompt || actionType === 'delete') confirm(); return; } return false; });
  return <Modal title={action === 'delete' ? 'Delete task' : prompt?.title ?? 'Loading prompt…'}><p className="prose">{action === 'delete' ? `Delete task "${task.name}"?` : prompt?.body}</p>{error && <p role="alert">{error}</p>}{action === 'delete' ? <div className="actions"><button disabled={busy} onClick={confirm}>Delete</button><button className="secondary" onClick={dismiss}>Cancel</button></div> : prompt && <PromptButtons prompt={prompt} disabled={busy} choose={choose} />}</Modal>;
}

export function Squad({ client, available, readOnly, submit, detach, enabled = true, onInterrupt, onAttachmentChange }: { client: DaemonClient; available: boolean; readOnly: boolean; submit: (args: string[]) => void; detach: () => void; enabled?: boolean; onInterrupt?: () => void; onAttachmentChange?: (attached: boolean) => void }) {
  const [tasks, setTasks] = useState<SquadTask[]>([]); const [selected, setSelected] = useState(0);
  const [detail, setDetail] = useState<SquadTask | null>(null); const [runs, setRuns] = useState<SquadRun[] | null>(null);
  const [error, setError] = useState(''); const [unavailable, setUnavailable] = useState(!available);
  const [action, setAction] = useState<TaskAction | null>(null); const [target, setTarget] = useState<SquadTask | null>(null);
  const [busy, setBusy] = useState(false);
  const [columns, setColumns] = useState(1);
  useEffect(() => {
    if (typeof matchMedia !== 'function') return; const media = matchMedia('(min-width: 768px)');
    const update = () => setColumns(media.matches ? 2 : 1); update(); media.addEventListener('change', update); return () => media.removeEventListener('change', update);
  }, []);
  const [attachOwner, setAttachOwner] = useState<string | null>(null); const [attach, setAttach] = useState<string | null>(null); const [detachConfirm, setDetachConfirm] = useState(false);
  useEffect(() => { onAttachmentChange?.(!!attachOwner); return () => onAttachmentChange?.(false); }, [attachOwner, onAttachmentChange]);
  useEffect(() => {
    if (!available) return;
    let live = true; let polling = false;
    async function refresh() {
      if (!live || polling || document.visibilityState === 'hidden') return; polling = true;
      try { const items = await client.squad.tasks(); if (live) { setTasks(items); setError(''); setUnavailable(false); } }
      catch (e) { if (live) { setError(errorText(e)); if (e && typeof e === 'object' && 'status' in e && (e.status === 404 || e.status === 503)) setUnavailable(true); } }
      finally { polling = false; }
    }
    void refresh(); const timer = setInterval(() => void refresh(), 2000);
    const visibility = () => { if (document.visibilityState === 'visible') void refresh(); };
    document.addEventListener('visibilitychange', visibility);
    return () => { live = false; clearInterval(timer); document.removeEventListener('visibilitychange', visibility); };
  }, [available, client]);
  useEffect(() => {
    if (!attachOwner) return;
    let live = true; let checking = false;
    const refresh = async () => {
      if (checking || document.visibilityState === 'hidden') return; checking = true;
      try { const terminals = await client.terminals(attachOwner); if (live) setAttach(terminals.find(t => t.state === 'open')?.id ?? null); }
      catch (e) { if (live) setError(errorText(e)); } finally { checking = false; }
    };
    void refresh(); const timer = setInterval(() => void refresh(), 2000);
    return () => { live = false; clearInterval(timer); };
  }, [client, attachOwner]);
  const task = tasks[Math.min(selected, Math.max(0, tasks.length - 1))];
  async function perform(work: () => Promise<unknown>) {
    if (busy || readOnly) return; setBusy(true); setError('');
    try { await work(); setTasks(await client.squad.tasks()); setAction(null); setTarget(null); } catch (e) { setError(errorText(e)); } finally { setBusy(false); }
  }
  async function details() { if (!task) return; try { setDetail(await client.squad.task(task.id)); } catch (e) { setError(errorText(e)); } }
  async function history() { if (!task) return; try { setRuns(await client.squad.runs(task.id)); } catch (e) { setError(errorText(e)); } }
  async function attachTask() {
    if (!task || readOnly) return;
    try { if (attachOwner !== task.id) { submit(['squad', 'attach', task.name]); setAttachOwner(task.id); } }
    catch (e) { setError(errorText(e)); }
  }
  async function catalogueCommand(action: string, name?: string) { if (readOnly) return; try { submit(await actionArgs(client, action, name)); } catch (e) { setError(errorText(e)); } }
  // One detach routine for the button and the keyboard: release the terminal
  // socket (unmounting TerminalPanel), stop discovery polling (owner cleared)
  // and leave the task itself running, so Attach can start over.
  function confirmDetach() { detach(); setAttach(null); setAttachOwner(null); setDetachConfirm(false); }
  function request(a: TaskAction) { if (task && !readOnly) { setTarget(task); setAction(a); } }
  function confirm() {
    if (!action || !target) return;
    void perform(async () => {
      if (action === 'pause') await client.squad.patch(target.id, { paused: true });
      if (action === 'trigger') await client.squad.trigger(target.id);
      if (action === 'delete') await client.squad.delete(target.id);
      if (action === 'cancel') { const current = await client.squad.runs(target.id); const run = current.find(r => r.status === 'running'); if (!run) throw new Error('No running task run to cancel.'); await client.squad.cancelRun(run.id); }
    });
  }
  useShortcuts(() => ({ context: detachConfirm ? 'dialog' : 'squadList', opts: detachConfirm ? { dialog: 'yes_no' } : {} }), a => {
    if (action || detail || runs) return false;
    if (detachConfirm) { if (a === 'dialog.yes') { confirmDetach(); return; } if (a === 'dialog.no' || a === 'dialog.dismiss') { setDetachConfirm(false); return; } return false; }
    if (a.startsWith('nav.')) { setSelected(i => Math.max(0, Math.min(tasks.length - 1, i + (a === 'nav.up' ? -columns : a === 'nav.down' ? columns : a === 'nav.left' ? -1 : a === 'nav.pageUp' ? -10 : a === 'nav.pageDown' ? 10 : 1)))); return; }
    if (a === 'squad.detail') { void details(); return; } if (a === 'squad.history') { void history(); return; }
    if (a === 'squad.attach') { void attachTask(); return; } if (a === 'containers.detach' || a === 'interrupt') { if (attachOwner) setDetachConfirm(true); else if (a === 'interrupt') onInterrupt?.(); return; }
    if (a === 'squad.new') { void catalogueCommand('new_squad_task'); return; }
    if (a === 'squad.edit' && task) { void catalogueCommand('edit_squad_task', task.name); return; }
    if (a === 'squad.resume' && task) { void perform(() => client.squad.patch(task.id, { paused: false })); return; }
    const actions: Record<string, TaskAction> = { 'squad.pause': 'pause', 'squad.trigger': 'trigger', 'squad.cancel': 'cancel', 'squad.delete': 'delete' };
    const next = actions[a]; if (next) { request(next); return; } return false;
  }, enabled);
  if (unavailable) return <section className="panel"><h2>Squad unavailable</h2><p>Enable squad on this listener and in the daemon, then reopen the squad tab.</p><pre>awman config set daemon.network.listeners.loopback.features engine,squad</pre><pre>awman config set daemon.features.squad.enabled true</pre>{error && <p role="alert">{error}</p>}</section>;
  return <section className="panel squad" data-key-context="squadList"><h2>Squad</h2><p className="hint">The command box is inactive in the squad tab. Use task controls.</p>{error && <p role="alert">{error}</p>}
    <button disabled={readOnly || busy} onClick={() => submit(['squad', 'add', '--interview'])}>New task</button>
    <VirtualList label="Squad tasks" activeIndex={Math.floor(selected / columns)} rowHeight={132} items={Array.from({ length: Math.ceil(tasks.length / columns) }, (_, row) => tasks.slice(row * columns, (row + 1) * columns))} render={(row, rowIndex) => <div className="squad-card-row">{row.map((t, col) => <button className="secondary squad-card" key={t.id} aria-pressed={selected === rowIndex * columns + col} onClick={() => { setSelected(rowIndex * columns + col); void client.squad.task(t.id).then(setDetail).catch(e => setError(errorText(e))); }}>{t.name} · {t.status} · {t.last_run_status ?? 'No runs'}{t.unmet_env.length ? ` · Missing ${t.unmet_env.join(', ')}` : ''}</button>)}</div>} />
    {task && <><h3>{task.name}</h3><p>{task.description}</p><div className="actions"><button onClick={() => void details()}>Details</button><button onClick={() => void history()}>History</button><button disabled={readOnly || busy} onClick={() => void attachTask()}>Attach</button><button disabled={readOnly || busy} onClick={() => submit(['squad', 'edit', task.name, '--interview'])}>Edit</button><button disabled={readOnly || busy || task.status === 'paused'} onClick={() => request('pause')}>Pause</button><button disabled={readOnly || busy || task.status === 'active'} onClick={() => void perform(() => client.squad.patch(task.id, { paused: false }))}>Resume</button><button disabled={readOnly || busy} onClick={() => request('trigger')}>Trigger</button><button disabled={readOnly || busy} onClick={() => request('cancel')}>Cancel run</button><button disabled={readOnly || busy} onClick={() => request('delete')}>Delete</button></div></>}
    {attachOwner && !attach && <p role="status">Waiting for the task terminal… <button onClick={() => setDetachConfirm(true)}>Detach</button></p>}
    {attach && <><button onClick={() => setDetachConfirm(true)}>Detach</button><TerminalPanel client={client} terminalId={attach} title={task?.name ?? 'Squad task'} readOnly={readOnly} /></>}
    {action && target && enabled && <TaskConfirm client={client} action={action} task={target} confirm={confirm} dismiss={() => { if (!busy) { setAction(null); setTarget(null); } }} busy={busy} />}
    {detail && <Modal title={detail.name} onDismiss={() => setDetail(null)}><p>{detail.description}</p><dl>{Object.entries(detail).filter(([k]) => k !== 'name' && k !== 'description').map(([k, v]) => <div key={k}><dt>{k}</dt><dd>{typeof v === 'string' ? v : JSON.stringify(v)}</dd></div>)}</dl></Modal>}
    {runs && <Modal title="Run history" onDismiss={() => setRuns(null)}><VirtualList label="Task runs" items={runs} render={r => <span>{r.started_at} · {r.status} · {r.error ?? r.reason}</span>} /></Modal>}
    {detachConfirm && <Modal title="Detach from task?"><p>The task and its agents keep running.</p><div className="actions"><button onClick={confirmDetach}>Detach</button><button className="secondary" onClick={() => setDetachConfirm(false)}>Keep attached</button></div></Modal>}
  </section>;
}
