'use client';
import { useEffect, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { SquadRun, SquadTask } from '@/lib/client/types';
import { useScreenShortcuts as useShortcuts } from './ui';
import { Modal, VirtualList, actionArgs, copyText, errorText } from './ui';
import { PromptButtons, usePrompt } from './Dialogs';
import { Menu } from '@/lib/shell/Menu';
import { TerminalPanel } from './TerminalPanel';

function RunHistory({ runs }: { runs: SquadRun[] }) {
  const [expanded, setExpanded] = useState<string | null>(null);
  return <div className="run-history" role="region" aria-label="Task runs">{!runs.length && <p>No runs yet.</p>}{runs.map(run => <article className="run-card" key={run.id}><button className="secondary" aria-expanded={expanded === run.id} onClick={() => setExpanded(expanded === run.id ? null : run.id)}>{run.started_at || 'Start time unavailable'} · {run.status}</button>{expanded === run.id && <dl>{[['Started', run.started_at], ['Finished', run.finished_at], ['Status', run.status], ['Reason', run.reason], ['Error', run.error], ['Unmet environment', run.unmet_env?.join(', ')], ['Workflow', run.workflow_path], ['Session', run.session_id]].map(([label, value]) => <div key={label}><dt>{label}</dt><dd>{value || '—'}</dd></div>)}</dl>}</article>)}</div>;
}

type TaskAction = 'pause' | 'trigger' | 'cancel' | 'delete';
function TaskConfirm({ client, action, task, confirm, dismiss, busy }: { client: DaemonClient; action: TaskAction; task: SquadTask; confirm: () => void; dismiss: () => void; busy: boolean }) {
  const actionType = action;
  const { prompt, error } = usePrompt(client, action === 'delete' ? null : 'squad_task_confirm', { action, name: task.name });
  function choose(value: string) { if (value === 'dispatch' || value === 'yes') confirm(); else dismiss(); }
  useShortcuts(() => ({ context: 'dialog', opts: { dialog: 'yes_no' } }), action => { if (action === 'dialog.dismiss' || action === 'dialog.no') { dismiss(); return; } if (action === 'dialog.yes') { if (prompt || actionType === 'delete') confirm(); return; } return false; });
  return <Modal title={action === 'delete' ? 'Delete task' : prompt?.title ?? 'Loading prompt…'}><p className="prose">{action === 'delete' ? `Delete task "${task.name}"?` : prompt?.body}</p>{error && <p role="alert">{error}</p>}{action === 'delete' ? <div className="actions"><button className="secondary danger" disabled={busy} onClick={confirm}>Delete</button><button className="secondary" onClick={dismiss}>Cancel</button></div> : prompt && <PromptButtons prompt={prompt} disabled={busy} choose={choose} />}</Modal>;
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
  const mutationReason = readOnly ? 'Attach as primary to change tasks.' : busy ? 'Waiting for the task action.' : undefined;
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
  function confirmDetach() { if (readOnly) return; detach(); setAttach(null); setAttachOwner(null); setDetachConfirm(false); }
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
    if (action || runs) return false;
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
  if (unavailable) return <section className="panel"><h2>Squad unavailable</h2><p>Enable squad on this listener and in the daemon, then reopen the squad tab.</p><div className="stack">{['awman config set daemon.network.listeners.loopback.features engine,squad', 'awman config set daemon.features.squad.enabled true'].map(command => <div className="squad-command" key={command}><pre>{command}</pre><button className="secondary" aria-label={"Copy " + command} onClick={() => void copyText(command).catch(() => setError('Copy failed.'))}>Copy</button></div>)}</div>{error && <p role="alert">{error}</p>}</section>;
  return <section className="panel squad" data-key-context="squadList"><header><h2>Squad</h2><button className={task ? 'secondary' : ''} disabled={readOnly || busy} onClick={() => void catalogueCommand('new_squad_task')}>New task</button></header><p className="hint">The command box is inactive in the squad tab. Use task controls.</p>{error && <p role="alert">{error}</p>}

    <VirtualList label="Squad tasks" activeIndex={Math.floor(selected / columns)} rowHeight={132} items={Array.from({ length: Math.ceil(tasks.length / columns) }, (_, row) => tasks.slice(row * columns, (row + 1) * columns))} render={(row, rowIndex) => <div className="squad-card-row">{row.map((t, col) => <button className="secondary squad-card" key={t.id} aria-pressed={selected === rowIndex * columns + col} onClick={() => { setSelected(rowIndex * columns + col); void client.squad.task(t.id).then(setDetail).catch(e => setError(errorText(e))); }}><strong>{t.name}</strong><span>{t.status} · {t.last_run_status ?? 'No runs'}</span><span className="hint">Every {Math.max(1, Math.round(t.interval_ms / 60000))} minutes</span>{t.unmet_env.length ? ` · Missing ${t.unmet_env.join(', ')}` : ''}</button>)}</div>} />
    {task && <section className="task-detail" aria-label="Selected task"><header><h3>{task.name}</h3><Menu label="Task actions" actions={[
      { id: 'detail', label: 'Details', group: 'Workspace', matrix: ['squad.detail'], run: () => { void details(); } },
      { id: 'history', label: 'History', group: 'Workspace', matrix: ['squad.history'], run: () => { void history(); } },
      { id: 'edit', label: 'Edit', group: 'Workspace', matrix: ['squad.edit'], disabled: mutationReason, run: () => { void catalogueCommand('edit_squad_task', task.name); } },
      { id: 'pause', label: task.status === 'paused' ? 'Resume' : 'Pause', group: 'Workspace', matrix: ['squad.pause', 'squad.resume'], disabled: mutationReason, run: () => { if (task.status === 'paused') void perform(() => client.squad.patch(task.id, { paused: false })); else request('pause'); } },
      { id: 'trigger', label: 'Trigger', group: 'Workspace', matrix: ['squad.trigger'], disabled: mutationReason, run: () => request('trigger') },
      { id: 'attach', label: 'Attach', group: 'Workspace', matrix: ['squad.attach'], disabled: mutationReason, run: () => { void attachTask(); } },
      { id: 'cancel', label: 'Cancel run', group: 'Danger', matrix: ['squad.cancel'], disabled: mutationReason, run: () => request('cancel') },
      { id: 'delete', label: 'Delete task', group: 'Danger', matrix: ['squad.delete'], disabled: mutationReason, run: () => request('delete') },
    ]} /></header><p>{task.description}</p><button disabled={readOnly || busy} onClick={() => { if (task.last_run_status === 'running') void attachTask(); else request('trigger'); }}>{task.last_run_status === 'running' ? 'Attach' : 'Trigger'}</button>
      {detail?.id === task.id && <div className="task-properties"><header><h3>Task details</h3><button className="secondary" onClick={() => setDetail(null)}>Close details</button></header><p>{detail.description}</p><dl>{Object.entries(detail).filter(([k]) => k !== 'name' && k !== 'description').map(([k, v]) => <div key={k}><dt>{k.replaceAll('_', ' ')}</dt><dd>{typeof v === 'string' ? v : JSON.stringify(v)}</dd></div>)}</dl></div>}
    </section>}
    {attachOwner && !attach && <p role="status">Waiting for the task terminal… <button className="secondary" disabled={readOnly} onClick={() => setDetachConfirm(true)}>Detach</button></p>}
    {attach && <section className="squad-terminal"><header><h3>Attached task</h3><button className="secondary" disabled={readOnly} onClick={() => setDetachConfirm(true)}>Detach</button></header><TerminalPanel client={client} terminalId={attach} title={task?.name ?? 'Squad task'} readOnly={readOnly} /></section>}
    {action && target && enabled && <TaskConfirm client={client} action={action} task={target} confirm={confirm} dismiss={() => { if (!busy) { setAction(null); setTarget(null); } }} busy={busy || readOnly} />}
    {runs && <Modal title="Run history" onDismiss={() => setRuns(null)}><RunHistory runs={runs} /></Modal>}
    {detachConfirm && <Modal title="Detach from task?"><p>The task and its agents keep running.</p><div className="actions"><button disabled={readOnly} onClick={confirmDetach}>Detach</button><button className="secondary" onClick={() => setDetachConfirm(false)}>Keep attached</button></div></Modal>}
  </section>;
}
