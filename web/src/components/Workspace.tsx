'use client';
import { useCallback, useEffect, useRef, useState, useSyncExternalStore, type KeyboardEvent as ReactKeyboardEvent } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { FrontendConnection, OutboundFrame } from '@/lib/client/frontendSocket';
import type { ContainerWindowStateWire, EffectiveStatus, LaunchContextSummary, PickRun, PromptKind, UiRequest } from '@/lib/client/types';
import { hasRunningWork } from '@/lib/client/reducers';
import { liveCountdownDialog } from '@/lib/client/countdown';
import type { OpenDialog } from '@/lib/client/instanceStore';
import { ShellLayout, isNarrowLayout } from '@/lib/shell/ShellLayout';
import { StagePanes } from '@/lib/shell/StagePanes';
import { CommandMenu } from '@/lib/shell/CommandMenu';
import { Menu } from '@/lib/shell/Menu';
import { catalogueActions, fillComposer, invokeAction, type ShellAction } from '@/lib/shell/actions';
import type { Catalogue } from '@/lib/client/types';
import type { InspectorKind, NarrowPanel } from '@/lib/shell/panels';
import { isTextInput } from '@/lib/keys/useShortcuts';
import { useScreenShortcuts as useShortcuts } from './ui';
import { map as mapKey, shortcutSheet } from '@/lib/keys/keymap';
import { clearResume, writeResume, readResume } from '@/lib/state/session';
import { DialogRenderer, dialogScope, PromptButtons, usePrompt } from './Dialogs';
import { Composer } from './Composer';
import { AgentSelector, ContainerArea, MediaQuery, ExecutionLog, GitPanel, NoticeExpiry, SlotBar, StageSwitch, TabBar, WorkflowPanel } from './Panels';
import { estimatePtySize } from '@/terminal/geometry';
import { Squad } from './Squad';
import { QueuePanel } from './QueuePanel';
import { Modal, SheetPresentation, actionArgs, copyText, selectedText, errorText, useObservable } from './ui';
import { answeredByText, connectionLabel, phaseLabel } from './presentation';
import { closedConnectionView } from '@/screens/recovery/closedConnection';
import { throttleText, useRecoveryThrottle } from '@/screens/recovery/useRecoveryThrottle';

function NewTab({ client, connection, cwd, squadId, focus, dismiss, readOnly }: { client: DaemonClient; connection: FrontendConnection; cwd: string; readOnly: boolean; squadId?: string; focus: (id: string) => void; dismiss: () => void }) {
  const [contexts, setContexts] = useState<LaunchContextSummary[]>([]); const [root, setRoot] = useState(''); const [path, setPath] = useState(cwd);
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  const { prompt, error: promptError } = usePrompt(client, 'new_tab');
  useEffect(() => { let live = true; void client.launchContexts().then(list => { if (!live) return; setContexts(list.items); const c = list.items.find(c => cwd === c.root || cwd.startsWith(c.root + '/')) ?? list.items.find(c => c.id === list.default_id) ?? list.items[0]; setRoot(c?.id ?? ''); if (!cwd) setPath(c?.root ?? ''); }).catch(e => { if (live) setError(errorText(e)); }); return () => { live = false; }; }, [client, cwd]);
  useEffect(() => connection.onFrame(frame => {
    if (frame.type === 'delta' && frame.delta === 'tab_opened') dismiss();
    if (frame.type === 'notice' && frame.level === 'error') { setError(frame.text); setBusy(false); }
  }), [connection, dismiss]);
  function open(squad = false) {
    if (busy || readOnly) return;
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
  return <Modal title={prompt?.title ?? 'New tab'}><p>{prompt?.body}</p>{promptError && <p role="alert">{promptError}</p>}{error && <p role="alert">{error}</p>}<label>Root<select aria-label="Root" value={root} onChange={e => { setRoot(e.target.value); setPath(contexts.find(c => c.id === e.target.value)?.root ?? ''); }}>{contexts.map(c => <option value={c.id} key={c.id}>{c.label} · {c.root}</option>)}</select></label><label>Path<input aria-label="Path" value={path} onChange={e => setPath(e.target.value)} /></label><p className="hint">Directories must stay inside an authorized workspace root.</p>{busy && <p role="status">Waiting for the engine to open the tab…</p>}<div className="actions"><button disabled={readOnly || busy || !path || !root} onClick={() => open()}>Open</button><button className="secondary" disabled={readOnly || busy} onClick={() => open(true)}>Open squad</button><button className="secondary" onClick={dismiss}>{error ? 'Dismiss' : 'Cancel'}</button></div></Modal>;
}
function LocalConfirm({ client, kind, confirm, dismiss, readOnly }: { client: DaemonClient; kind: PromptKind; readOnly: boolean; confirm: (choice: string) => void; dismiss: () => void }) {
  const { prompt, error } = usePrompt(client, kind);
  useShortcuts(() => ({ context: 'dialog', opts: { dialog: kind === 'close_tab_confirm' ? 'close_tab_confirm' : kind === 'quit_confirm' ? 'quit_confirm' : 'yes_no' } }), a => {
    if (a === 'dialog.dismiss' || a === 'dialog.no') { dismiss(); return; }
    if (!prompt || readOnly) return false;
    if (a === 'dialog.quit') { confirm('quit'); return; } if (a === 'dialog.closeTab') { confirm('close_tab'); return; }
    if (a === 'dialog.yes') { confirm('abort'); return; } return false;
  });
  return <Modal title={prompt?.title ?? 'Loading prompt…'}>{error && <p role="alert">{error}</p>}{prompt && <><p className="prose">{prompt.body}</p>{kind === 'quit_confirm' && <p className="hint">Closing the browser tab disconnects this frontend. The engine allows 60 seconds to reconnect before applying its primary exit policy.</p>}<fieldset className="panel-guard" disabled={readOnly}><PromptButtons prompt={prompt} choose={confirm} /></fieldset></>}<button className="secondary" onClick={dismiss}>Cancel</button></Modal>;
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
  const [narrowPanel, setNarrowPanel] = useState<NarrowPanel>('output');
  const [inspector, setInspector] = useState<InspectorKind>(null);
  const [stageMode, setStageMode] = useState<'agent' | 'output'>('agent');
  const [executionMinimized, setExecutionMinimized] = useState<Record<string, boolean>>({});
  const [commands, setCommands] = useState(false); const [tabList, setTabList] = useState(false);
  const [catalogue, setCatalogue] = useState<Catalogue | null>(null);
  const [catalogueError, setCatalogueError] = useState('');
  // A rate-limited reauthorization proof is temporary: say it is retrying.
  const throttled = useRecoveryThrottle(client);
  useEffect(() => { let live = true; void client.catalogue().then(c => { if (live) setCatalogue(c); }).catch(e => { if (live) setCatalogueError(errorText(e)); }); return () => { live = false; }; }, [client]);
  const workflow = inspector === 'workflow';
  const [showStatus, setShowStatus] = useState(false); const [shortcuts, setShortcuts] = useState(false); const [newTab, setNewTab] = useState(false);
  const [confirm, setConfirm] = useState<{ kind: PromptKind; tabId: string } | null>(null); const [queue, setQueue] = useState(false);
  const [queueOwner, setQueueOwner] = useState<string | null>(null);
  const [queueBadge, setQueueBadge] = useState({ newItems: 0, forYou: 0 });
  const [leaseConfirm, setLeaseConfirm] = useState<{ tabId: string; leaseId: string; title: string } | null>(null);
  const [squadAttached, setSquadAttached] = useState(false);
  const [focused, setFocused] = useState<Record<string, string>>({}); const [windows, setWindows] = useState<Record<string, ContainerWindowStateWire>>({});
  const [dismissedNotices, setDismissedNotices] = useState<Set<string>>(new Set());
  const [copyNotice, setCopyNotice] = useState(''); const [forceCancel, setForceCancel] = useState(false);
  const [dismissedAnswers, setDismissedAnswers] = useState<ReadonlySet<string>>(new Set());
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
  const countdown = dialog?.request.kind === 'workflow_yolo_countdown';
  // The countdown is a nonmodal banner: only a real engine dialog blocks the
  // workspace (navigation, Actions, Queue, Squad, Stop, Cancel workflow).
  const engineModal = !!dialog && !countdown;
  const engineModalId = engineModal ? dialog.dialogId : null;
  // A local sheet hidden behind an engine dialog must not reappear after it.
  useEffect(() => { if (engineModalId) { setCommands(false); setTabList(false); setShortcuts(false); setNewTab(false); } }, [engineModalId]);
  const modal = engineModal || newTab || !!confirm || !!leaseConfirm || shortcuts || commands || tabList;
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
  function openQueue() { if (!queue) setQueueOwner(tab?.tab_id ?? null); setQueueBadge({ newItems: 0, forYou: 0 }); setQueue(true); setInspector('queue'); setNarrowPanel('more'); }
  const dismissNewTab = useCallback(() => setNewTab(false), []);
  const connectionMount = useRef(0);
  const mountedConnection = useRef(connection);
  useEffect(() => {
    const mount = ++connectionMount.current;
    mountedConnection.current = connection;
    return () => {
      // Strict Mode replays effect cleanup during the initial mount. Defer the
      // close so its next setup can keep the socket that is still connecting.
      queueMicrotask(() => {
        // The live ref is intentional: the next setup cancels this cleanup.
        // eslint-disable-next-line react-hooks/exhaustive-deps
        if (connectionMount.current === mount || mountedConnection.current !== connection) connection.close('navigation');
      });
    };
  }, [connection]);
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
  function send(frame: OutboundFrame) { if (connection.role.get() !== 'primary' || connection.state.get() !== 'open') return; try { connection.send(frame); } catch (e) { connection.store.notice('error', errorText(e), 'client'); } }
  function ui(request: UiRequest) { if (connection.role.get() !== 'primary' || connection.state.get() !== 'open') return; void connection.uiRequest(request).then(outcome => { if (outcome.detail) connection.store.notice('info', outcome.detail, 'client'); }).catch(e => connection.store.notice('error', errorText(e), 'client')); }
  function select(id: string) { setActive(id); if (!readOnly) send({ type: 'focus_tab', tab_id: id }); }
  function switchTab(direction: number) { if (!tab) return; const next = tabs[(tabs.indexOf(tab) + direction + tabs.length) % tabs.length]; if (next) { select(next.tab_id); ui({ request: direction > 0 ? 'next_tab' : 'prev_tab' }); } }
  function activity() { if (tab && !readOnly && Date.now() - activityAt.current >= 1000) { activityAt.current = Date.now(); send({ type: 'activity', tab_id: tab.tab_id }); } }
  function cancel() { if (!tab) return; send({ type: 'cancel', tab_id: tab.tab_id, repeat: forceCancel }); setForceCancel(true); }
  useEffect(() => { setForceCancel(false); }, [tab?.tab_id, tab?.execution_phase.phase]);
  function submit(args: string[]) { if (tab) send({ type: 'submit', tab_id: tab.tab_id, input: { source: 'argv', args }, pty_size: estimatePtySize() }); }
  function submitAction(action: string) { void actionArgs(client, action).then(submit).catch(e => connection.store.notice('error', errorText(e), 'client')); }
  function control(step?: string) { if (!tab) return; send({ type: 'workflow_request', tab_id: tab.tab_id, request: { request: 'open_control_board', step_name: step ?? focusedSlot?.step_name ?? tab.workflow?.current_step ?? '' } }); }
  function setWindow(id: string, state: ContainerWindowStateWire) { if (!tab) return; setStageMode(state === 'maximized' ? 'agent' : 'output'); setWindows(w => ({ ...w, [id]: state })); ui({ request: 'set_container_window', tab_id: tab.tab_id, slot_id: id, state }); }
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
  function openOverview() { if (queue) return; setInspector(workflow ? null : 'workflow'); setNarrowPanel(workflow ? 'output' : 'workflow'); if (!workflow && tab) { ui({ request: 'open_overview', tab_id: tab.tab_id }); ui({ request: 'focus_workflow_window', tab_id: tab.tab_id }); } }
  /** Ctrl+C only: copy a selection if there is one (TUI parity), else stop. */
  function interrupt() {
    if (selectedText()) { copy(); return; }
    stop();
  }
  /** The Stop button and Actions → Stop always take the stop path. */
  function stop() {
    if (!tab) return;
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
    if (readOnly) return;
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
      connection.send({ type: 'submit', tab_id: opened, input: { source: 'lease', lease_id: run.lease_id, args: run.argv }, pty_size: estimatePtySize() });
      select(opened);
    } catch (error) {
      try { if (tab) await connection.queueRequest(queueOwner ?? tab.tab_id, { operation: 'finish', lease_id: run.lease_id, choice: 'release' }); } catch { /* engine will retire the lease */ }
      if (opened) { try { connection.send({ type: 'close_tab', tab_id: opened }); } catch { /* connection may be gone */ } }
      throw error;
    }
  }
  function closeInspector() { if (queue) return; setInspector(null); setNarrowPanel('output'); }
  function choosePanel(panel: NarrowPanel, slotId = focusedSlot?.slot_id) {
    if (queue && (panel === 'workflow' || panel === 'git')) return;
    setNarrowPanel(panel);
    if (panel === 'agent' || panel === 'output') setStageMode(panel);
    if (panel === 'output' && tab) setExecutionMinimized(v => ({ ...v, [tab.tab_id]: false }));
    if (panel === 'agent' && slotId && (windows[slotId] ?? 'maximized') !== 'maximized') setWindow(slotId, 'maximized');
    // Only the narrow layout shows one main panel at a time. Where the
    // inspector docks or overlays, Agent/Output change the stage alone.
    if (!queue && (panel === 'workflow' || panel === 'git' || isNarrowLayout())) setInspector(panel === 'workflow' || panel === 'git' ? panel : null);
  }
  /** Shift+←/→ in the open workflow inspector: select the neighbouring step. */
  function selectStep(direction: -1 | 1) {
    const rows = [...document.querySelectorAll<HTMLButtonElement>('[aria-label="Workflow steps"] button[aria-expanded]')];
    const at = rows.findIndex(row => row.getAttribute('aria-expanded') === 'true');
    const next = rows[at < 0 ? (direction > 0 ? 0 : rows.length - 1) : Math.max(0, Math.min(rows.length - 1, at + direction))];
    if (next && next.getAttribute('aria-expanded') !== 'true') next.click();
    next?.focus();
  }
  function openGit() { if (queue) return; setInspector(inspector === 'git' ? null : 'git'); setNarrowPanel(inspector === 'git' ? 'output' : 'git'); }
  function openSquad() {
    const existing = tabs.find(t => t.kind === 'squad');
    if (existing) { select(existing.tab_id); choosePanel('output'); }
    else if (tab && !readOnly) {
      send({ type: 'open_tab', dir: tab.session.cwd, options: { kind: 'squad', annotations: {}, startup: 'ready' } });
      ui({ request: 'open_tab', dir: tab.session.cwd });
    }
  }
  const mutationReason = engineModal ? 'Answer the engine prompt first.' : readOnly ? state !== 'open' ? 'Reconnecting — actions are read only.' : 'Attach as primary to use this action.' : undefined;
  const runReason = mutationReason ?? (tab?.execution_phase.phase === 'running' ? 'Stop the running command first.' : tab?.kind === 'squad' ? 'Choose a command tab to run a command.' : undefined);
  const queueReason = queue ? 'Close the queue before opening another inspector.' : undefined;
  const actions: ShellAction[] = [
    { id: 'tab.new', shortcut: 'tab.new', label: 'New tab', group: 'Workspace', matrix: ['repo.new_tab_dialog'], disabled: mutationReason, run: () => setNewTab(true) },
    { id: 'tab.prev', shortcut: 'tab.prev', label: 'Previous tab', group: 'Go to', matrix: ['tabs.prev'], run: () => switchTab(-1) },
    { id: 'tab.next', shortcut: 'tab.next', label: 'Next tab', group: 'Go to', matrix: ['tabs.next'], run: () => switchTab(1) },
    { id: 'tabs', label: 'All tabs', group: 'Go to', matrix: ['nav.tab_list'], run: () => setTabList(true) },
    { id: 'squad', label: 'Squad', group: 'Go to', matrix: ['repo.new_tab_squad', 'tabs.squad_singleton'], disabled: tabs.some(t => t.kind === 'squad') ? undefined : mutationReason, run: openSquad },
    { id: 'agent', label: 'Active agent', group: 'Go to', matrix: ['nav.active_agent_chip'], run: () => choosePanel('agent') },
    { id: 'output', shortcut: 'composer.focusExecution', label: 'Execution output', group: 'Go to', matrix: ['cmd.focus_execution'], run: () => { choosePanel('output'); requestAnimationFrame(() => document.querySelector<HTMLElement>('[aria-label="Command output"]')?.focus()); } },
    { id: 'git.toggle', shortcut: 'git.toggle', label: 'Git changes', group: 'Go to', matrix: ['git.toggle'], disabled: queueReason, run: openGit },
    { id: 'workflow.overview', shortcut: 'workflow.overview', label: 'Workflow overview', group: 'Go to', matrix: ['wf.overview_toggle'], disabled: queueReason, run: openOverview },
    { id: 'queue.open', shortcut: 'queue.open', label: 'Work queue', group: 'Go to', matrix: ['queue.panel'], run: openQueue },
    { id: 'config.open', shortcut: 'config.open', label: 'Settings', group: 'Workspace', matrix: ['config.open'], disabled: runReason, run: () => submitAction('show_config') },
    { id: 'shortcuts.open', shortcut: 'shortcuts.open', label: 'Keyboard shortcuts', group: 'Workspace', matrix: ['keys.shortcut_list'], run: () => setShortcuts(true) },
    { id: 'container.focusNextSlot', shortcut: 'container.focusNextSlot', label: 'Next agent', group: 'Agent', matrix: ['ctr.focus_next_slot'], disabled: tab?.slots.length ? undefined : 'No agents in this tab.', run: cycleSlot },
    { id: 'container.cycle', shortcut: 'container.cycle', label: 'Cycle agent window', group: 'Agent', matrix: ['ctr.window_cycle'], disabled: focusedSlot ? undefined : 'No active agent.', run: () => { if (focusedSlot) { const current = windows[focusedSlot.slot_id] ?? 'maximized'; setWindow(focusedSlot.slot_id, current === 'maximized' ? 'minimized' : current === 'minimized' ? 'hidden' : 'maximized'); } } },
    { id: 'containers.detach', shortcut: 'containers.detach', label: 'Collapse agent', group: 'Agent', matrix: ['ctr.detach'], disabled: focusedSlot ? undefined : 'No active agent.', run: detach },
    { id: 'workflow.control', shortcut: 'workflow.control', label: 'Control workflow', group: 'Workflow', matrix: ['wfc.open'], disabled: mutationReason ?? (tab?.workflow ? undefined : 'No workflow in this tab.'), run: () => control() },
    { id: 'workflow.copyContextPath', shortcut: 'workflow.copyContextPath', label: 'Copy workflow context path', group: 'Workflow', matrix: ['wf.context_path_copy'], disabled: tab?.context_path ? undefined : 'No workflow context path.', run: () => void copyText(tab?.context_path ?? '').then(() => setCopyNotice('Copied context path')).catch(() => setCopyNotice('Copy failed')) },
    // Keyboard accelerators of controls that are visible where they apply
    // (the step rows, the log's own scrolling): shortcuts only, not Actions.
    ...(['Left', 'Right'] as const).map(direction => ({ id: 'workflow.overview' + direction, shortcut: ('workflow.overview' + direction) as 'workflow.overviewLeft' | 'workflow.overviewRight', label: direction === 'Left' ? 'Previous workflow step' : 'Next workflow step', group: 'Workflow' as const, matrix: ['wf.overview_hscroll'], menu: false as const, disabled: workflow ? undefined : 'Open the workflow inspector first.', run: () => selectStep(direction === 'Left' ? -1 : 1) })),
    { id: 'selection.copy', shortcut: 'selection.copy', label: 'Copy selection', group: 'Output', matrix: ['exec.select_copy'], run: copy },
    { id: 'exec.toggleStatusLog', shortcut: 'exec.toggleStatusLog', label: 'Toggle status messages', group: 'Output', matrix: ['exec.status_log_toggle'], run: () => { choosePanel('output'); setShowStatus(v => !v); } },
    { id: 'exec.focusComposer', shortcut: 'exec.focusComposer', label: 'Focus command composer', group: 'Go to', matrix: ['exec.focus_back'], run: () => document.getElementById('command')?.focus() },
    ...(['scrollUp', 'scrollDown', 'pageUp', 'pageDown', 'top', 'bottom'] as const).map(kind => ({ id: 'exec.' + kind, shortcut: ('exec.' + kind) as 'exec.scrollUp', label: ({ scrollUp: 'Scroll output up', scrollDown: 'Scroll output down', pageUp: 'Previous page of output', pageDown: 'Next page of output', top: 'Top of output', bottom: 'End of output' })[kind], group: 'Output' as const, matrix: ['exec.scroll'], menu: false as const, run: () => { choosePanel('output'); requestAnimationFrame(() => { const log = document.querySelector('[aria-label="Command output"]'); if (kind === 'top' || kind === 'bottom') log?.scrollTo({ top: kind === 'top' ? 0 : log.scrollHeight }); else log?.scrollBy({ top: kind === 'scrollUp' ? -28 : kind === 'scrollDown' ? 28 : kind === 'pageUp' ? -400 : 400 }); }); } })),
    { id: 'exec.stopWatch', shortcut: 'exec.stopWatch', label: 'Stop watching agents', group: 'Output', matrix: ['exec.agents_watch_stop'], disabled: mutationReason ?? (tab?.agents_dashboard?.watching ? undefined : 'No agent watch is running.'), run: cancel },
    { id: 'interrupt', shortcut: 'interrupt', label: forceCancel ? 'Force stop command' : 'Stop command', group: 'Danger', matrix: ['quit.cancel_command'], disabled: mutationReason ?? (tab?.execution_phase.phase === 'running' ? undefined : 'No command is running.'), run: stop },
    { id: 'workflow.cancel', label: 'Cancel workflow', group: 'Danger', matrix: ['appr.cancel_workflow_confirm'], disabled: mutationReason ?? (tab?.workflow ? undefined : 'No workflow is running.'), run: () => { if (tab) setConfirm({ kind: 'workflow_cancel_confirm', tabId: tab.tab_id }); } },
    { id: 'tab.close', label: 'Close current tab', group: 'Danger', matrix: ['tabs.close_confirm'], disabled: mutationReason, run: () => { if (tab) askCloseTab(tab.tab_id); } },
    { id: 'quit', shortcut: 'composer.quit', label: 'Quit awman', group: 'Danger', matrix: ['cmd.q_quit', 'quit.confirm'], disabled: mutationReason, run: () => { if (tab) setConfirm({ kind: 'quit_confirm', tabId: tab.tab_id }); } },
    ...(catalogue ? catalogueActions(catalogue, fillComposer, runReason) : []),
  ];
  function act(id: string) { const action = actions.find(a => a.id === id); if (action) invokeAction(action); }
  useShortcuts(() => {
    if (dialog) return { context: 'dialog', opts: { dialog: dialogScope(dialog.request), containers: tab?.slots.length } };
    const element = document.activeElement;
    const context = element?.closest<HTMLElement>('[data-key-context]')?.dataset.keyContext;
    return { context: context === 'terminal' ? 'terminal' : context === 'execution' ? 'execution' : context === 'composer' ? 'composer' : context === 'squadList' ? 'squadList' : 'global', opts: { textInput: isTextInput(element), containers: tab?.slots.length, watching: !!tab?.agents_dashboard?.watching, composerEmpty: element instanceof HTMLTextAreaElement && element.id === 'command' && !element.value, overviewHScroll: workflow } };
  }, (a, event) => {
    if (!tab) return false;
    if (newTab || confirm || leaseConfirm || shortcuts || commands || tabList) { if (shortcuts && a === 'dialog.dismiss') setShortcuts(false); return false; }
    if (dialog && dialog.request.kind !== 'workflow_yolo_countdown' && dialog.request.kind !== 'workflow_step_confirm') return false;
    if (!dialog && document.querySelector('[role="dialog"]')) return false;
    if (a === 'tab.new' && !dialog) { act('tab.new'); return; }
    if (a === 'tab.prev' || a === 'tab.next') { act(a); return; }
    if (a === 'container.focusNextSlot') { act(a); return; }
    if (a === 'workflow.control') { act(a); return; }
    if (a === 'interrupt') { if (tab.kind === 'squad') return false; interrupt(); return; }
    if (dialog) return false;
    const registered = actions.find(action => action.shortcut === a);
    if (registered) { invokeAction(registered); return; }
    if (event.defaultPrevented) return; return false;
  });
  if (view.ended) {
    // Explain the ending with the entry flow's state-specific recovery copy.
    const ended = closedConnectionView(connection.closeReason ?? null, view.ended.reason);
    const label = ended.recovery === 'connect' ? 'Connect this browser again' : ended.recovery === 'choose_workspace' ? 'Choose another workspace' : ended.recovery === 'resume' ? 'Resume workspace' : 'Reconnect';
    // A resumable ending keeps the resume record, so the entry flow can offer
    // its options (read-only attach, explicit resume) again.
    const keepResume = ended.recovery === 'connect' || ended.recovery === 'resume';
    return <main className="connect"><section className="connect-card" role="alert" aria-labelledby="workspace-ended"><h1 id="workspace-ended">{ended.title}</h1><p>{ended.explanation}</p><div className="actions"><button onClick={() => { if (!keepResume) clearResume(); reconnect(); }}>{label}</button></div></section></main>;
  }
  if (!view.snapshot || !tab) return <main className="connect"><section className="connect-card"><h1>Opening workspace…</h1><p role="status">{connectionLabel(state)}</p><button className="secondary" onClick={reconnect}>Reconnect</button></section></main>;
  const entries = view.outputs.get(tab.tab_id) ?? [];
  const dialogUi = leaseConfirm
    ? <Modal title="Close running work item"><p>{leaseConfirm.title}</p><p>Choose what to do with the claimed work item.</p><div className="actions"><button disabled={readOnly} onClick={() => void finishLease('release')}>Release</button><button className="secondary danger" disabled={readOnly} onClick={() => void finishLease('cancel')}>Cancel run</button><button className="secondary" onClick={() => void finishLease('keep_running')}>Keep running</button></div></Modal>
    : confirm ? <LocalConfirm readOnly={readOnly} client={client} kind={confirm.kind} confirm={confirmChoice} dismiss={() => setConfirm(null)} />
    : dialog && !countdown ? <DialogRenderer key={`${view.generation}:${dialog.dialogId}`} dialog={dialog} client={client} connection={connection} readOnly={readOnly} onLocalDismiss={() => setDismissedNotices(ids => new Set(ids).add(dialog.dialogId))} />
    : newTab ? <NewTab readOnly={readOnly} client={client} connection={connection} cwd={tab.session.cwd} squadId={tabs.find(t => t.kind === 'squad')?.tab_id} focus={select} dismiss={dismissNewTab} />
    : shortcuts ? <Shortcuts close={() => setShortcuts(false)} />
    : commands ? <CommandMenu actions={actions} close={() => setCommands(false)} />
    : tabList ? <Modal title="Workspace tabs" onDismiss={() => setTabList(false)}><div className="tab-list">{tabs.map(t => <button className="secondary" key={t.tab_id} aria-pressed={t.tab_id === tab.tab_id} onClick={() => { select(t.tab_id); setTabList(false); }}>{t.label.text} · {phaseLabel(t.execution_phase.phase)}{t.stuck ? ' · Stuck' : ''}</button>)}<button disabled={readOnly || engineModal} onClick={() => { setTabList(false); setNewTab(true); }}>New tab</button><button className="secondary danger" disabled={readOnly} onClick={() => { setTabList(false); askCloseTab(tab.tab_id); }}>Close current tab</button></div></Modal>
    : null;
  const more = <div className="more-actions">{actions.filter(a => ['squad', 'queue.open', 'config.open', 'shortcuts.open', 'quit'].includes(a.id)).map(action => <div className="command-option" key={action.id}><button className={action.group === 'Danger' ? 'secondary danger' : 'secondary'} disabled={!!action.disabled} onClick={() => invokeAction(action)}>{action.label}</button>{action.disabled && <p className="hint">{action.disabled}</p>}</div>)}</div>;
  const controlAction = actions.find(a => a.id === 'workflow.control'); const cancelAction = actions.find(a => a.id === 'workflow.cancel');
  // Cancel workflow and Control workflow follow their actions exactly; a
  // disabled one says why beside it, not inside the dimmed button.
  const workflowActions = <div className="inspector-actions">{[controlAction, cancelAction].map(action => action && <div className="command-option" key={action.id}><button className={action.group === 'Danger' ? 'secondary danger' : 'secondary'} disabled={!!action.disabled} onClick={() => invokeAction(action)}>{action.label}</button>{action.disabled && <p className="hint">{action.disabled}</p>}</div>)}</div>;
  const agentVisible = stageMode === 'agent' && !!focusedSlot && (windows[focusedSlot.slot_id] ?? 'maximized') === 'maximized';
  return <div className="workspace-root" onKeyDownCapture={captureKey} onPointerDownCapture={activity} onScrollCapture={activity}>
    <ShellLayout inspector={inspector} narrowPanel={narrowPanel} onNarrowPanelChange={choosePanel} onInspectorClose={closeInspector} disabledPanels={queue ? { git: 'Close the queue first.', workflow: 'Close the queue first.' } : undefined} attention={{ agent: !!focusedSlot?.stuck, more: queueBadge.newItems > 0 }} escapeNeedsFocus={countdown} slots={{
      appBar: <><div className="desktop-tabs"><TabBar tabs={tabs} active={tab.tab_id} select={select} close={askCloseTab} newTab={() => act('tab.new')} readOnly={readOnly || engineModal} /></div>{tabs.length > 1 && <button className="secondary all-tabs" onClick={() => setTabList(true)}>All tabs ({tabs.length})</button>}<button className="secondary current-tab" onClick={() => setTabList(true)} title={tab.label.text}><span>{tab.label.text}</span><span aria-hidden="true">▾</span></button><button className="secondary mobile-new" aria-label="New tab" disabled={readOnly || engineModal} onClick={() => act('tab.new')}>+</button><div className="app-actions"><button className="secondary desktop-action" disabled={engineModal || (!tabs.some(t => t.kind === 'squad') && readOnly)} onClick={openSquad}><span className={`squad-dot squad-${view.snapshot.squad_health}`} aria-hidden="true" />Squad<span className="sr-only">: {view.snapshot.squad_health}</span></button><button className="secondary desktop-action" aria-pressed={inspector === 'queue'} disabled={engineModal} onClick={openQueue}>Queue{queueBadge.newItems + queueBadge.forYou > 0 ? ` · ${queueBadge.newItems + queueBadge.forYou}` : ''}</button><button className="secondary" disabled={engineModal} onClick={() => setCommands(true)}>Actions</button><div className="desktop-action"><Menu label="App menu" actions={actions.filter(a => ['config.open', 'shortcuts.open', 'quit'].includes(a.id))} /></div><div className="desktop-action"><Menu label="Tab menu" actions={actions.filter(a => ['tabs', 'tab.close'].includes(a.id))} /></div></div></>,
      contextBar: <><div className="context-path" title={tab.session.cwd + (tab.worktree_path ? ' · ' + tab.worktree_path : '')}>{tab.session.cwd}{tab.worktree_path && <span> · {tab.worktree_path}</span>}</div><button className="secondary context-chip" disabled={!!queueReason} aria-pressed={inspector === 'git'} onClick={openGit}>Git · {tab.git_summary?.branch ?? 'No branch'} · {tab.git_summary?.files.length ?? 0}</button><button className="secondary context-chip" disabled={!!queueReason} aria-pressed={workflow} onClick={openOverview}>Workflow{tab.workflow ? ` · ${tab.workflow.current_step}` : ''}</button><span className="connection-pill" role="status">{connectionLabel(state)}{role === 'primary' ? '' : ' · View only'}</span></>,
      activeAgent: <button className="secondary" onClick={() => choosePanel('agent')}>{tab.kind === 'squad' && squadAttached ? 'Active agent · squad task' : focusedSlot ? <SlotBar slot={focusedSlot} /> : 'Active agent · none'}</button>,
      stage: <div className="stage-content" data-stage={agentVisible ? 'agent' : 'output'}><MediaQuery query="(max-width: 767px)">{narrow => {
        // Keep the minimize control and agent selector in the terminal header.
        // When it is closed, the stage heading still offers agent selection.
        const inHeader = agentVisible && tab.kind !== 'squad' && (narrowPanel === 'agent' || !narrow);
        const minimizeContainer = <button className="secondary panel-minimize" aria-label="Minimize container" title="Minimize container" onClick={() => { if (focusedSlot) { setWindow(focusedSlot.slot_id, 'minimized'); setNarrowPanel('output'); } }}>−</button>;
        const stageSwitch = <StageSwitch agent={agentVisible} agentDisabled={!focusedSlot} showAgent={() => choosePanel('agent')} showOutput={() => choosePanel('output')} />;
        const selector = (slots: typeof tab.slots) => <AgentSelector slots={slots} value={focusedSlot?.slot_id ?? ''} choose={id => { setFocused(v => ({ ...v, [tab.tab_id]: id })); choosePanel('agent', id); }} />;
        const banner = countdown && dialog ? <SheetPresentation.Provider value="banner"><DialogRenderer key={`${view.generation}:${dialog.dialogId}`} dialog={dialog} client={client} connection={connection} readOnly={readOnly} onLocalDismiss={() => setDismissedNotices(ids => new Set(ids).add(dialog.dialogId))} /></SheetPresentation.Provider> : null;
        const interactive = !modal && !countdown && agentVisible && (narrowPanel === 'agent' || !narrow);
        // Terminals of every tab stay mounted (hidden) so switching keeps their connection and control.
        const agentTabs = tabs.filter(t => t.kind !== 'squad' && t.slots.length);
        return <>{!inHeader && <div className="stage-heading">{stageSwitch}{tab.slots.length > 0 && selector(tab.slots)}</div>}
        {!inHeader && banner}
        <NoticeExpiry notices={view.notices} dismiss={id => connection.store.dismissNotice(id)} />
        <StagePanes inert={modal} executionOpen={!executionMinimized[tab.tab_id] || tab.kind === 'squad'} containersOpen={agentVisible}
          minimized={<>{executionMinimized[tab.tab_id] && tab.kind !== 'squad' && <button className="secondary" onClick={() => { setExecutionMinimized(v => ({ ...v, [tab.tab_id]: false })); setNarrowPanel('output'); }}>Restore Execution</button>}{tab.slots.filter(s => (windows[s.slot_id] ?? 'maximized') !== 'maximized' || (s.slot_id === focusedSlot?.slot_id && !agentVisible)).map(s => <button className="secondary" key={s.slot_id} onClick={() => { setFocused(v => ({ ...v, [tab.tab_id]: s.slot_id })); setWindow(s.slot_id, 'maximized'); setNarrowPanel('agent'); }}>Restore Container{tab.slots.length > 1 ? ` · ${s.phase_step_title ?? s.step_name}` : ''}</button>)}</>}
          containers={<>{agentTabs.map(t => { const current = t.tab_id === tab.tab_id; return <ContainerArea key={t.tab_id} hidden={!current} client={client} readOnly={readOnly} viewOnly={role === 'secondary'} tab={t} focused={current ? focusedSlot?.slot_id ?? null : t.slots.find(s => s.slot_id === focused[t.tab_id])?.slot_id ?? t.focused_slot} setFocused={id => setFocused(v => ({ ...v, [t.tab_id]: id }))} windows={windows} interactive={current && interactive} setWindow={setWindow} lead={current ? <>{minimizeContainer}{t.slots.length > 1 && selector(t.slots)}</> : undefined} overlay={current && inHeader ? banner : undefined} />; })}{tab.kind !== 'squad' && !tab.slots.length && <p className="empty-state">No active agent in this tab. Run a command to get started.</p>}</>}
          execution={tab.kind === 'squad' ? <Squad key={tab.tab_id} client={client} available={squadAvailable} readOnly={readOnly} submit={submit} detach={detach} onInterrupt={interrupt} onAttachmentChange={setSquadAttached} enabled={!modal} /> : <ExecutionLog tab={tab} entries={entries} showStatus={showStatus} toggleStatus={() => setShowStatus(v => !v)} stopWatch={cancel} copy={copy} readOnly={readOnly} minimize={() => { setExecutionMinimized(v => ({ ...v, [tab.tab_id]: true })); if (agentVisible) setNarrowPanel('agent'); }} />} />
        </>;
      }}</MediaQuery></div>,
      composer: <div className="composer-dock" inert={modal}>{tab.kind === 'squad' ? <p className="hint">Squad task controls are above. The command box is inactive.</p> : <><Composer key={tab.tab_id} client={client} connection={connection} tab={tab} readOnly={readOnly} entries={entries} quit={() => act('quit')} copy={copy} />{tab.execution_phase.phase === 'running' && <button className="composer-stop danger" disabled={readOnly} onClick={stop}>{forceCancel ? 'Force stop' : 'Stop'}</button>}</>}</div>,
      inspector: queue ? <SheetPresentation.Provider value="drawer">{queueAvailable ? <QueuePanel connection={connection} tabId={queueOwner ?? tab.tab_id} readOnly={readOnly} onPick={openPicked} onClose={() => { setQueue(false); setInspector(null); setNarrowPanel('output'); }} /> : <Modal title="Work queue" onDismiss={() => { setQueue(false); setInspector(null); setNarrowPanel('output'); }}><p>Update awman to use the work queue in the browser.</p></Modal>}</SheetPresentation.Provider> : inspector === 'git' ? <GitPanel tab={tab} close={closeInspector} /> : <>{workflowActions}<WorkflowPanel tab={tab} close={closeInspector} control={control} readOnly={readOnly || engineModal} /></>,
      more,
      dialogs: dialogUi,
      notices: <>{view.notices.slice(-5).map(n => <div className="notice" role={n.level === 'error' ? 'alert' : 'status'} key={n.id}><p>{n.text}</p><button className="secondary" onClick={() => connection.store.dismissNotice(n.id)}>Dismiss</button></div>)}{[...view.closedDialogs].slice(-1).filter(([id]) => !dismissedAnswers.has(id)).map(([id, by]) => { const text = answeredByText(by, connection.id); return text && <div className="notice" role="status" key={id}><p>{text}</p><button className="secondary" onClick={() => setDismissedAnswers(ids => new Set(ids).add(id))}>Dismiss</button></div>; })}{copyNotice && <div className="notice" role="status">{copyNotice}<button className="secondary" onClick={() => setCopyNotice('')}>Dismiss</button></div>}{throttled !== null && <div className="notice" role="status"><p>{throttleText(throttled)}</p></div>}{catalogueError && <p className="notice" role="alert">Commands could not load: {catalogueError}</p>}</>,
    }} />
  </div>;
}
