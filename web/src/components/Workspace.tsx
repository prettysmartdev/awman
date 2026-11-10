'use client';
import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { FrontendConnection, OutboundFrame } from '@/lib/client/frontendSocket';
import type { ContainerWindowStateWire, EffectiveStatus, LaunchContextSummary, PickRun, PromptKind, UiRequest } from '@/lib/client/types';
import { hasRunningWork } from '@/lib/client/reducers';
import { liveCountdownDialog } from '@/lib/client/countdown';
import type { OpenDialog } from '@/lib/client/instanceStore';
import { ShellLayout } from '@/lib/shell/ShellLayout';
import type { NarrowPanel } from '@/lib/shell/panels';
import { isTextInput } from '@/lib/keys/useShortcuts';
import { useScreenShortcuts as useShortcuts } from './ui';
import { map as mapKey, shortcutSheet } from '@/lib/keys/keymap';
import { clearResume, writeResume, readResume } from '@/lib/state/session';
import { DialogRenderer, dialogScope, PromptButtons, usePrompt } from './Dialogs';
import { Composer } from './Composer';
import { ContainerArea, ExecutionLog, GitPanel, SlotBar, TabBar, WorkflowPanel } from './Panels';
import { Squad } from './Squad';
import { QueuePanel } from './QueuePanel';
import { Modal, actionArgs, copyText, selectedText, errorText, useObservable } from './ui';

function NewTab({ client, connection, cwd, squadId, focus, dismiss }: { client: DaemonClient; connection: FrontendConnection; cwd: string; squadId?: string; focus: (id: string) => void; dismiss: () => void }) {
  const [contexts, setContexts] = useState<LaunchContextSummary[]>([]); const [root, setRoot] = useState(''); const [path, setPath] = useState(cwd);
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  const { prompt, error: promptError } = usePrompt(client, 'new_tab');
  useEffect(() => { let live = true; void client.launchContexts().then(list => { if (!live) return; setContexts(list.items); const c = list.items.find(c => cwd === c.root || cwd.startsWith(c.root + '/')) ?? list.items.find(c => c.id === list.default_id) ?? list.items[0]; setRoot(c?.id ?? ''); if (!cwd) setPath(c?.root ?? ''); }).catch(e => { if (live) setError(errorText(e)); }); return () => { live = false; }; }, [client, cwd]);
  useEffect(() => connection.onFrame(frame => {
    if (frame.type === 'delta' && frame.delta === 'tab_opened') dismiss();
    if (frame.type === 'notice' && frame.level === 'error') { setError(frame.text); setBusy(false); }
  }), [connection, dismiss]);
  function open(squad = false) {
    if (busy) return;
    if (squad && squadId) { focus(squadId); dismiss(); return; }
    try {
      connection.send({ type: 'open_tab', dir: path, options: { kind: squad ? 'squad' : 'normal', annotations: {}, startup: 'ready' } });
      void connection.uiRequest({ request: 'open_tab', dir: path }).catch(e => setError(errorText(e)));
      setBusy(true); setError('');
    } catch (e) { setError(errorText(e)); }
  }
  useShortcuts(() => ({ context: 'dialog', opts: { dialog: 'new_tab' } }), a => {
    if (a === 'dialog.dismiss') { dismiss(); return; } if (a === 'dialog.submit') { open(); return; } if (a === 'newTab.squad') { open(true); return; } return false;
  });
  return <Modal title={prompt?.title ?? 'New tab'}><p>{prompt?.body}</p>{promptError && <p role="alert">{promptError}</p>}{error && <p role="alert">{error}</p>}<label>Root<select aria-label="Root" value={root} onChange={e => { setRoot(e.target.value); setPath(contexts.find(c => c.id === e.target.value)?.root ?? ''); }}>{contexts.map(c => <option value={c.id} key={c.id}>{c.label} · {c.root}</option>)}</select></label><label>Path<input aria-label="Path" value={path} onChange={e => setPath(e.target.value)} /></label><p className="hint">Directories must stay inside an authorized workspace root.</p>{busy && <p role="status">Waiting for the engine to open the tab…</p>}<div className="actions"><button disabled={busy || !path || !root} onClick={() => open()}>Open</button><button disabled={busy} onClick={() => open(true)}>Open squad</button><button className="secondary" onClick={dismiss}>{error ? 'Dismiss' : 'Cancel'}</button></div></Modal>;
}
function LocalConfirm({ client, kind, confirm, dismiss }: { client: DaemonClient; kind: PromptKind; confirm: (choice: string) => void; dismiss: () => void }) {
  const { prompt, error } = usePrompt(client, kind);
  useShortcuts(() => ({ context: 'dialog', opts: { dialog: kind === 'close_tab_confirm' ? 'close_tab_confirm' : kind === 'quit_confirm' ? 'quit_confirm' : 'yes_no' } }), a => {
    if (a === 'dialog.dismiss' || a === 'dialog.no') { dismiss(); return; }
    if (!prompt) return false;
    if (a === 'dialog.quit') { confirm('quit'); return; } if (a === 'dialog.closeTab') { confirm('close_tab'); return; }
    if (a === 'dialog.yes') { confirm('abort'); return; } return false;
  });
  return <Modal title={prompt?.title ?? 'Loading prompt…'}>{error && <p role="alert">{error}</p>}{prompt && <><p className="prose">{prompt.body}</p>{kind === 'quit_confirm' && <p className="hint">Closing the browser tab disconnects this frontend. The engine allows 60 seconds to reconnect before applying its primary exit policy.</p>}<PromptButtons prompt={prompt} choose={confirm} /></>}<button className="secondary" onClick={dismiss}>Cancel</button></Modal>;
}
export function Shortcuts({ close }: { close: () => void }) {
  return <Modal title="Keyboard shortcuts" onDismiss={close}><p>Browser and OS reserved keys keep their normal behavior. Use the alternatives or visible controls.</p><div className="shortcut-list">{shortcutSheet().map((s, i) => <div key={i}><strong>{s.label}</strong><span>{s.alternates.length ? s.alternates.join(' / ') : s.keys.join(' / ') || 'Button'}{s.reserved.length ? ` · reserved: ${s.reserved.join(', ')}` : ''} · {s.group}</span></div>)}</div></Modal>;
}

export function Workspace({ client, connection, launchContext, squadAvailable, queueAvailable = true, reconnect }: { client: DaemonClient; connection: FrontendConnection; launchContext: string | null; squadAvailable: boolean; queueAvailable?: boolean; reconnect: () => void }) {
  const view = useSyncExternalStore(connection.store.subscribe, connection.store.get, connection.store.get);
  const state = useObservable(connection.state); const role = useObservable(connection.role);
  const readOnly = role !== 'primary' || state !== 'open';
  const [active, setActive] = useState<string | null>(readResume()?.tabId ?? null);
  const [programStatuses, setProgramStatuses] = useState<{ generation: number; slots: ReadonlyMap<string, EffectiveStatus | null> }>({ generation: -1, slots: new Map() });
  useEffect(() => connection.onFrame(frame => {
    if (frame.type !== 'program_status') return;
    const current = connection.store.get();
    const liveSlots = new Set(current.snapshot?.tabs.flatMap(t => t.slots.map(s => s.slot_id)) ?? []);
    if (!liveSlots.has(frame.slot_id)) return;
    setProgramStatuses(previous => ({ generation: current.generation, slots: new Map([...(previous.generation === current.generation ? previous.slots : new Map())].filter(([id]) => liveSlots.has(id))).set(frame.slot_id, frame.status) }));
  }), [connection]);
  // ProgramStatus is an authoritative event feed outside structural deltas.
  // Project it onto the rendered snapshot, discarding it on every full resync.
  const tabs = (view.snapshot?.tabs ?? []).map(t => ({ ...t, label: t.annotations['awman.lease_title'] ? { ...t.label, text: `${t.annotations['awman.lease_title']} · ${t.annotations['awman.lease_server'] ?? 'work server'}` } : t.label, slots: t.slots.map(slot => programStatuses.generation === view.generation && programStatuses.slots.has(slot.slot_id) ? { ...slot, program_status: programStatuses.slots.get(slot.slot_id) ?? null } : slot) })); const tab = tabs.find(t => t.tab_id === active) ?? tabs.find(t => t.tab_id === view.snapshot?.active_tab_hint) ?? tabs[0];
  const [narrowPanel, setNarrowPanel] = useState<NarrowPanel>('log'); const [git, setGit] = useState(false); const [workflow, setWorkflow] = useState(false);
  const [showStatus, setShowStatus] = useState(false); const [shortcuts, setShortcuts] = useState(false); const [newTab, setNewTab] = useState(false);
  const [confirm, setConfirm] = useState<{ kind: PromptKind; tabId: string } | null>(null); const [queue, setQueue] = useState(false);
  const [queueBadge, setQueueBadge] = useState({ newItems: 0, forYou: 0 });
  const [leaseConfirm, setLeaseConfirm] = useState<{ tabId: string; leaseId: string; title: string } | null>(null);
  const [squadAttached, setSquadAttached] = useState(false);
  const [focused, setFocused] = useState<Record<string, string>>({}); const [windows, setWindows] = useState<Record<string, ContainerWindowStateWire>>({});
  const [dismissedNotices, setDismissedNotices] = useState<Set<string>>(new Set());
  const [copyNotice, setCopyNotice] = useState(''); const [forceCancel, setForceCancel] = useState(false);
  const activityAt = useRef(0);
  // Keep engine order within the active tab. Background prompts stay pending
  // and become visible on tab selection, without answering or reordering them.
  const focusedSlot = tab?.slots.find(s => s.slot_id === focused[tab.tab_id]) ?? tab?.slots.find(s => s.slot_id === tab.focused_slot) ?? tab?.slots[0];
  // Precedence: answering engine dialogs first (engine order), then a
  // countdown for the focused slot's step, then the live countdown derived
  // from Yolo/SlotYolo state (TUI parity), then any other countdown request.
  const tabDialogs = view.dialogs.filter(d => d.tabId === tab?.tab_id && !dismissedNotices.has(d.dialogId));
  const isCountdown = (d: OpenDialog) => d.request.kind === 'workflow_yolo_countdown';
  const dialog = tabDialogs.find(d => !isCountdown(d))
    ?? tabDialogs.find(d => d.request.kind === 'workflow_yolo_countdown' && d.request.step_name === focusedSlot?.step_name)
    ?? (tab ? liveCountdownDialog(tab, focusedSlot, id => windows[id] ?? 'maximized') : null)
    ?? tabDialogs[0];
  const modal = !!dialog || newTab || !!confirm || !!leaseConfirm || shortcuts || queue;
  const queueTabId = tab?.tab_id;
  useEffect(() => {
    if (!queueAvailable || !queueTabId || state !== 'open') return;
    let live = true;
    const refresh = () => { void connection.queueRequest(queueTabId, { operation: 'snapshot' }).then(reply => {
      if (live && reply.result === 'snapshot') setQueueBadge(queue ? { newItems: 0, forYou: 0 } : { newItems: reply.value.new_since_seen, forYou: reply.value.for_you });
    }).catch(() => undefined); };
    refresh();
    const timer = setInterval(refresh, 5000);
    return () => { live = false; clearInterval(timer); };
  }, [connection, queueAvailable, queue, state, queueTabId]);
  function openQueue() { setQueueBadge({ newItems: 0, forYou: 0 }); setQueue(true); }
  const dismissNewTab = useCallback(() => setNewTab(false), []);
  useEffect(() => () => connection.close('navigation'), [connection]);
  useEffect(() => {
    if (connection.id && view.snapshot && role === 'primary') writeResume({ instanceId: view.snapshot.instance_id, frontendId: connection.id, tabId: tab?.tab_id ?? null, launchContext });
  }, [connection, role, view.snapshot, tab?.tab_id, launchContext]);
  // Warn from live (delta-maintained) execution state, never a stale count.
  const busy = hasRunningWork(view.snapshot);
  useEffect(() => {
    if (!busy) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ''; };
    window.addEventListener('beforeunload', warn); return () => window.removeEventListener('beforeunload', warn);
  }, [busy]);
  function send(frame: OutboundFrame) { if (readOnly) return; try { connection.send(frame); } catch (e) { connection.store.notice('error', errorText(e), 'client'); } }
  function ui(request: UiRequest) { if (readOnly) return; void connection.uiRequest(request).then(outcome => { if (outcome.detail) connection.store.notice('info', outcome.detail, 'client'); }).catch(e => connection.store.notice('error', errorText(e), 'client')); }
  function select(id: string) { setActive(id); if (!readOnly) send({ type: 'focus_tab', tab_id: id }); }
  function switchTab(direction: number) { if (!tab) return; const next = tabs[(tabs.indexOf(tab) + direction + tabs.length) % tabs.length]; if (next) { select(next.tab_id); ui({ request: direction > 0 ? 'next_tab' : 'prev_tab' }); } }
  function activity() { if (tab && !readOnly && Date.now() - activityAt.current >= 1000) { activityAt.current = Date.now(); send({ type: 'activity', tab_id: tab.tab_id }); } }
  function cancel() { if (!tab) return; send({ type: 'cancel', tab_id: tab.tab_id, repeat: forceCancel }); setForceCancel(true); }
  useEffect(() => { setForceCancel(false); }, [tab?.tab_id, tab?.execution_phase.phase]);
  function submit(args: string[]) { if (tab) send({ type: 'submit', tab_id: tab.tab_id, input: { source: 'argv', args }, pty_size: { cols: 100, rows: 30 } }); }
  function submitAction(action: string) { void actionArgs(client, action).then(submit).catch(e => connection.store.notice('error', errorText(e), 'client')); }
  function control(step?: string) { if (!tab) return; send({ type: 'workflow_request', tab_id: tab.tab_id, request: { request: 'open_control_board', step_name: step ?? focusedSlot?.step_name ?? tab.workflow?.current_step ?? '' } }); }
  function setWindow(id: string, state: ContainerWindowStateWire) { if (!tab) return; setWindows(w => ({ ...w, [id]: state })); ui({ request: 'set_container_window', tab_id: tab.tab_id, slot_id: id, state }); }
  function cycleSlot() {
    if (!tab) return;
    const index = tab.slots.findIndex(s => s.slot_id === focusedSlot?.slot_id);
    const next = tab.slots[(index + 1) % tab.slots.length];
    if (next) setFocused(v => ({ ...v, [tab.tab_id]: next.slot_id }));
  }
  function captureKey(event: ReactKeyboardEvent) {
    activity();
    // The contract has no container count. Intercept the centralized map's
    // slot-cycle action before a focused terminal can forward Ctrl+S.
    if (!modal && (event.target as HTMLElement).closest('[data-key-context="terminal"]') && mapKey(event, 'terminal', { containers: tab?.slots.length }) === 'container.focusNextSlot') {
      cycleSlot(); event.preventDefault(); event.stopPropagation();
    }
  }
  function detach() { if (tab?.kind === 'squad') send({ type: 'cancel', tab_id: tab.tab_id, repeat: false }); else if (focusedSlot) setWindow(focusedSlot.slot_id, 'minimized'); }
  function copy() { const text = selectedText(); if (text) void copyText(text).then(() => setCopyNotice('Copied selection')).catch(() => setCopyNotice('Copy failed')); }
  function openOverview() { setWorkflow(v => !v); if (!workflow && tab) { ui({ request: 'open_overview', tab_id: tab.tab_id }); ui({ request: 'focus_workflow_window', tab_id: tab.tab_id }); } }
  function interrupt() {
    if (!tab) return;
    if (selectedText()) { copy(); return; }
    if (tab.agents_dashboard?.watching) { cancel(); return; }
    if (tab.workflow) { setConfirm({ kind: 'workflow_cancel_confirm', tabId: tab.tab_id }); return; }
    if (tab.execution_phase.phase === 'running') { cancel(); return; }
    setConfirm({ kind: tabs.length > 1 ? 'close_tab_confirm' : 'quit_confirm', tabId: tab.tab_id });
  }
  function confirmChoice(choice: string) {
    if (!confirm) return;
    if (choice === 'quit' || (choice === 'yes' && confirm.kind === 'quit_confirm')) send({ type: 'quit' });
    if (choice === 'close_tab' || choice === 'close') { send({ type: 'close_tab', tab_id: confirm.tabId }); ui({ request: 'close_tab', tab_id: confirm.tabId }); }
    if ((choice === 'abort' || choice === 'yes' || choice === 'cancel_workflow') && confirm.kind === 'workflow_cancel_confirm') send({ type: 'cancel', tab_id: confirm.tabId, repeat: false });
    setConfirm(null);
  }
  function askCloseTab(id: string) {
    const closing = tabs.find(t => t.tab_id === id);
    const leaseId = closing?.annotations['awman.lease_id'];
    if (leaseId && closing?.execution_phase.phase === 'running') {
      setLeaseConfirm({ tabId: id, leaseId, title: closing.annotations['awman.lease_title'] ?? 'work item' });
    } else {
      setConfirm({ kind: tabs.length > 1 ? 'close_tab_confirm' : 'quit_confirm', tabId: id });
    }
  }
  async function finishLease(choice: 'release' | 'cancel' | 'keep_running') {
    if (!leaseConfirm) return;
    if (choice === 'keep_running') { setLeaseConfirm(null); return; }
    try {
      const reply = await connection.queueRequest(leaseConfirm.tabId, { operation: 'finish', lease_id: leaseConfirm.leaseId, choice });
      if (reply.result !== 'done') throw new Error(reply.result === 'error' ? reply.value.error ?? 'The lease could not be updated.' : 'Unexpected engine response.');
      send({ type: 'close_tab', tab_id: leaseConfirm.tabId });
      ui({ request: 'close_tab', tab_id: leaseConfirm.tabId });
      setLeaseConfirm(null);
    } catch (error) { connection.store.notice('error', errorText(error), 'client'); }
  }
  async function openPicked(run: PickRun) {
    let opened: string | null = null;
    try {
      if (readOnly) throw new Error('Only the primary browser can pick work.');
      if (!run.argv.length) throw new Error('This daemon did not include the picked command arguments. Update awman and try again.');
      const marker = crypto.randomUUID();
      opened = await new Promise<string>((resolve, reject) => {
        const timer = setTimeout(() => { unsubscribe(); reject(new Error('The engine did not open the picked workspace.')); }, 15_000);
        const unsubscribe = connection.onFrame(frame => {
          if (frame.type === 'delta' && frame.delta === 'tab_opened' && frame.value.annotations['awman.queue_request'] === marker) {
            clearTimeout(timer); unsubscribe(); resolve(frame.value.tab_id);
          }
        });
        try {
          connection.send({ type: 'open_tab', dir: run.cwd, options: { kind: 'normal', annotations: { 'awman.queue_request': marker, 'awman.lease_id': run.lease_id, 'awman.lease_title': run.title, 'awman.lease_server': run.server }, startup: 'none', lease_id: run.lease_id } });
        } catch (error) { clearTimeout(timer); unsubscribe(); reject(error); }
      });
      connection.send({ type: 'submit', tab_id: opened, input: { source: 'lease', lease_id: run.lease_id, args: run.argv }, pty_size: { cols: 100, rows: 30 } });
      select(opened);
    } catch (error) {
      try { if (tab) await connection.queueRequest(tab.tab_id, { operation: 'finish', lease_id: run.lease_id, choice: 'release' }); } catch { /* engine will retire the lease */ }
      if (opened) { try { connection.send({ type: 'close_tab', tab_id: opened }); } catch { /* connection may be gone */ } }
      throw error;
    }
  }
  useShortcuts(() => {
    if (dialog) return { context: 'dialog', opts: { dialog: dialogScope(dialog.request), containers: tab?.slots.length } };
    const element = document.activeElement;
    const context = element?.closest<HTMLElement>('[data-key-context]')?.dataset.keyContext;
    return { context: context === 'terminal' ? 'terminal' : context === 'execution' ? 'execution' : context === 'composer' ? 'composer' : context === 'squadList' ? 'squadList' : 'global', opts: { textInput: isTextInput(element), containers: tab?.slots.length, watching: !!tab?.agents_dashboard?.watching, composerEmpty: element instanceof HTMLTextAreaElement && element.id === 'command' && !element.value, overviewHScroll: workflow } };
  }, (a, event) => {
    if (!tab) return false;
    if (newTab || confirm || leaseConfirm || shortcuts || queue) { if (shortcuts && a === 'dialog.dismiss') setShortcuts(false); return false; }
    if (dialog && dialog.request.kind !== 'workflow_yolo_countdown' && dialog.request.kind !== 'workflow_step_confirm') return false;
    if (!dialog && document.querySelector('[role="dialog"]')) return false;
    if (a === 'tab.new' && !dialog) { if (!readOnly) setNewTab(true); return; }
    if (a === 'tab.prev') { switchTab(-1); return; } if (a === 'tab.next') { switchTab(1); return; }
    if (a === 'container.focusNextSlot') { cycleSlot(); return; }
    if (a === 'workflow.control') { control(); return; }
    if (a === 'interrupt') { if (tab.kind === 'squad') return false; interrupt(); return; }
    if (dialog) return false;
    if (a === 'git.toggle') { setGit(v => !v); return; }
    if (a === 'workflow.overview') { openOverview(); return; }
    if (a === 'workflow.overviewLeft' || a === 'workflow.overviewRight') { document.querySelector('[aria-label="Workflow steps"]')?.scrollBy({ left: a === 'workflow.overviewLeft' ? -240 : 240 }); return; }
    if (a === 'config.open') { submitAction('show_config'); return; }
    if (a === 'queue.open') { openQueue(); return; } if (a === 'shortcuts.open') { setShortcuts(true); return; }
    if (a === 'selection.copy') { copy(); return; }
    if (a === 'containers.detach') { if (tab.kind === 'squad') return false; detach(); return; }
    if (a === 'container.cycle') { if (focusedSlot) { const current = windows[focusedSlot.slot_id] ?? 'maximized'; setWindow(focusedSlot.slot_id, current === 'maximized' ? 'minimized' : current === 'minimized' ? 'hidden' : 'maximized'); } return; }
    if (a === 'exec.stopWatch') { cancel(); return; }
    if (a === 'exec.toggleStatusLog') { setShowStatus(v => !v); return; }
    if (a === 'exec.focusComposer') { document.getElementById('command')?.focus(); return; }
    if (a.startsWith('exec.')) { const log = document.querySelector('[aria-label="Command output"]'); if (!log) return false; const amount = a === 'exec.scrollUp' ? -28 : a === 'exec.scrollDown' ? 28 : a === 'exec.pageUp' ? -400 : 400; if (a === 'exec.top') log.scrollTo({ top: 0 }); else if (a === 'exec.bottom') log.scrollTo({ top: log.scrollHeight }); else log.scrollBy({ top: amount }); return; }
    if (event.defaultPrevented) return; return false;
  });
  if (view.ended) return <main className="connect"><section className="connect-card" role="alert"><h1>Engine disconnected</h1><p>{view.ended.reason}</p><button onClick={() => { clearResume(); reconnect(); }}>Reconnect</button></section></main>;
  if (!view.snapshot || !tab) return <main className="connect"><section className="connect-card"><h1>Opening workspace…</h1><p role="status">{state}</p><button onClick={reconnect}>Reconnect</button></section></main>;
  const entries = view.outputs.get(tab.tab_id) ?? [];
  const dialogUi = leaseConfirm
    ? <Modal title="Close running work item"><p>{leaseConfirm.title}</p><p>Choose what to do with the claimed work item.</p><div className="actions"><button onClick={() => void finishLease('release')}>Release</button><button onClick={() => void finishLease('cancel')}>Cancel run</button><button className="secondary" onClick={() => void finishLease('keep_running')}>Keep running</button></div></Modal>
    : confirm ? <LocalConfirm client={client} kind={confirm.kind} confirm={confirmChoice} dismiss={() => setConfirm(null)} />
    : dialog ? <DialogRenderer key={`${view.generation}:${dialog.dialogId}`} dialog={dialog} client={client} connection={connection} readOnly={readOnly} onLocalDismiss={() => setDismissedNotices(ids => new Set(ids).add(dialog.dialogId))} />
    : newTab ? <NewTab client={client} connection={connection} cwd={tab.session.cwd} squadId={tabs.find(t => t.kind === 'squad')?.tab_id} focus={select} dismiss={dismissNewTab} />
    : shortcuts ? <Shortcuts close={() => setShortcuts(false)} />
    : queue ? queueAvailable
      ? <QueuePanel connection={connection} tabId={tab.tab_id} readOnly={readOnly} onPick={openPicked} onClose={() => setQueue(false)} />
      : <Modal title="Work queue" onDismiss={() => setQueue(false)}><p>Update awman to use the work queue in the browser.</p></Modal>
    : null;
  const tools = <div className="workspace-tools"><button className="secondary" aria-pressed={git} onClick={() => setGit(v => !v)}>Git</button><button className="secondary" aria-pressed={workflow} onClick={openOverview}>Overview</button><button className="secondary" disabled={readOnly || !tab.workflow} onClick={() => control()}>Workflow</button><button className="secondary" disabled={readOnly} onClick={() => submit(['config', 'show'])}>Config</button><button className="secondary" onClick={openQueue}>Queue{queueBadge.newItems > 0 || queueBadge.forYou > 0 ? ` · ${queueBadge.newItems} new · ${queueBadge.forYou} for you` : ''}</button><button className="secondary" onClick={() => setShortcuts(true)}>Help</button><button className="secondary" onPointerDown={e => e.preventDefault()} onClick={copy}>Copy</button>{tab.kind !== 'squad' && <button className="secondary" disabled={readOnly} onClick={interrupt}>{forceCancel ? 'Force cancel' : 'Cancel'}</button>}<button className="secondary" disabled={readOnly} onClick={() => setConfirm({ kind: 'quit_confirm', tabId: tab.tab_id })}>Quit awman</button></div>;
  return <div onKeyDownCapture={captureKey} onPointerDownCapture={activity} onScrollCapture={activity}>
    <ShellLayout gitOpen={git || narrowPanel === 'git'} workflowOpen={workflow || narrowPanel === 'workflow'} narrowPanel={narrowPanel} onNarrowPanelChange={setNarrowPanel} slots={{
      tabBar: <TabBar tabs={tabs} active={tab.tab_id} select={select} close={askCloseTab} newTab={() => setNewTab(true)} readOnly={readOnly} />,
      status: <><div className="workspace-status"><span>{state}{role === 'secondary' ? ' · Read only' : ''}</span><span>CWD: {tab.session.cwd}</span>{tab.worktree_path && <span>Worktree: {tab.worktree_path}</span>}<span><span className={`squad-dot squad-${view.snapshot.squad_health}`} aria-hidden="true" />Squad: {view.snapshot.squad_health}</span>{tab.git_summary && <span>{tab.git_summary.branch} · {tab.git_summary.files.length} files · +{tab.git_summary.added} −{tab.git_summary.removed}</span>}{tab.context_path && <button className="secondary" onClick={() => void copyText(tab.context_path ?? '').then(() => setCopyNotice('Copied context path')).catch(() => setCopyNotice('Copy failed'))}>Copy context path</button>}</div>{tools}</>,
      activeAgent: <button className="secondary" onClick={() => setNarrowPanel('agent')}>{tab.kind === 'squad' && squadAttached ? 'Active agent · squad task' : focusedSlot ? <SlotBar slot={focusedSlot} /> : 'Active agent · none'}</button>,
      containers: tab.kind !== 'squad' && tab.slots.length ? <><ContainerArea client={client} readOnly={readOnly} tab={tab} focused={focusedSlot?.slot_id ?? null} setFocused={id => setFocused(v => ({ ...v, [tab.tab_id]: id }))} windows={windows} interactive={!modal && (narrowPanel === 'agent' || typeof matchMedia !== 'function' || !matchMedia('(max-width: 767px)').matches)} setWindow={setWindow} /><button className="secondary" onClick={detach}>Detach</button></> : undefined,
      execution: tab.kind === 'squad' ? <Squad key={tab.tab_id} client={client} available={squadAvailable} readOnly={readOnly} submit={submit} detach={detach} onInterrupt={interrupt} onAttachmentChange={setSquadAttached} enabled={!modal} /> : <ExecutionLog tab={tab} entries={entries} showStatus={showStatus} toggleStatus={() => setShowStatus(v => !v)} stopWatch={cancel} copy={copy} />,
      composer: tab.kind === 'squad' ? <p className="hint">Squad task controls are above. The command box is inactive.</p> : <Composer key={tab.tab_id} client={client} connection={connection} tab={tab} readOnly={readOnly} entries={entries} quit={() => setConfirm({ kind: 'quit_confirm', tabId: tab.tab_id })} copy={copy} />,
      git: <GitPanel tab={tab} close={() => { setGit(false); setNarrowPanel('log'); }} />,
      workflow: <WorkflowPanel tab={tab} close={() => { setWorkflow(false); setNarrowPanel('log'); }} control={control} />,
      shortcuts: tools,
      dialogs: dialogUi,
      notices: <>{view.notices.slice(-5).map(n => <div className="notice" role={n.level === 'error' ? 'alert' : 'status'} key={n.id}><p>{n.text}</p><button className="secondary" onClick={() => connection.store.dismissNotice(n.id)}>Dismiss</button></div>)}{[...view.closedDialogs].slice(-1).map(([id, by]) => <p className="notice" role="status" key={id}>Answered by {by.by === 'frontend' ? by.frontend_id : by.by}</p>)}{copyNotice && <p role="status">{copyNotice}</p>}</>,
    }} />
  </div>;
}
