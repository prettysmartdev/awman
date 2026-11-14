'use client';
import { Fragment, useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from 'react';
import type { OutputEntry } from '@/lib/client/instanceStore';
import type { EffectiveStatus, JsonValue, SessionUpdate, SlotSnapshot, TabSnapshot, WorkflowStepView } from '@/lib/client/types';
import type { PanelContext } from '@/lib/shell/panels';
import { Menu } from '@/lib/shell/Menu';
import type { ShellAction } from '@/lib/shell/actions';
import { countdownPresentation, phaseLabel } from './presentation';
import { TerminalPanel } from './TerminalPanel';
import { copyText, VirtualList } from './ui';

export function statusGlyph(s: EffectiveStatus | null | undefined): string {
  if (!s) return '';
  return `${s.state === 'working' ? '◐' : s.state === 'done' ? '✓' : s.state === 'error' ? '✗' : s.state === 'blocked' ? s.kind === 'question' ? '?' : '🔒' : '○'}${s.progress !== null ? ` ${s.progress}%` : ''} ${s.msg ?? ''}`;
}
export function SlotBar({ slot }: { slot: SlotSnapshot }) {
  return <span className="slot-summary"><strong>{slot.phase_step_title ?? slot.step_name} · {slot.agent_display_name}</strong> {slot.model} · {slot.container_name} {statusGlyph(slot.program_status)} {slot.stuck ? '⚠ Stuck' : ''} {slot.yolo ? countdownPresentation(slot.yolo.kind, slot.yolo.remaining_secs).title : ''} {slot.stats ? `CPU ${slot.stats.cpu_percent}% · ${slot.stats.memory_mb} MB` : ''}{slot.dormant ? ' · dormant' : ''}</span>;
}
export function TabBar({ tabs, active, select, close, newTab, readOnly }: { tabs: TabSnapshot[]; active: string | null; select: (id: string) => void; close: (id: string) => void; newTab: () => void; readOnly: boolean }) {
  const touch = useRef<number | null>(null);
  return <div className="tab-strip" onTouchStart={e => { touch.current = e.changedTouches[0]?.clientX ?? null; }} onTouchEnd={e => {
    const x = e.changedTouches[0]?.clientX; if (x === undefined || touch.current === null || Math.abs(x - touch.current) < 60) return;
    const i = tabs.findIndex(t => t.tab_id === active); const next = tabs[(i + (x > touch.current ? 1 : -1) + tabs.length) % tabs.length]; if (next) select(next.tab_id);
  }}><div role="tablist" aria-label="Workspace tabs">{tabs.map(t => <div className={`tab-item label-${t.label.color} ${t.kind === 'squad' ? 'squad-tab' : ''}`} key={t.tab_id}>
    <button role="tab" aria-selected={active === t.tab_id} title={`${t.label.text} · ${t.execution_phase.phase}${'command' in t.execution_phase ? `: ${t.execution_phase.command}` : ''}`} onClick={() => select(t.tab_id)}>{active === t.tab_id ? '➡ ' : ''}{t.label.text} {t.stuck ? '⚠' : ''} {statusGlyph(t.slots.find(s => s.slot_id === t.focused_slot)?.program_status ?? t.slots[0]?.program_status)}{t.yolo ? ` ${countdownPresentation(t.yolo.kind, t.yolo.remaining_secs).title}` : ''}</button>
    <button className="secondary" aria-label={`Close ${t.label.text}`} disabled={readOnly} onClick={() => close(t.tab_id)}>×</button>
  </div>)}</div><button className="secondary" aria-label="New tab" disabled={readOnly} onClick={newTab}>+</button></div>;
}
function outputText(entry: OutputEntry): string {
  if (entry.kind === 'text') return entry.text;
  // CommandBytes arrive already stream-decoded, ANSI-stripped and assembled
  // into lines by the InstanceStore.
  return entry.result.error ? `${entry.result.error.code}: ${entry.result.error.message}` : `Finished · exit ${entry.result.exit_code}`;
}
function Dashboard({ overview }: { overview: JsonValue }) {
  const object = overview && typeof overview === 'object' && !Array.isArray(overview) ? overview : {};
  const agents = Array.isArray(object.agents) ? object.agents : [];
  return <section aria-label="Agents dashboard"><h3>Agents dashboard</h3><table><caption>Runtime status</caption><tbody>{Object.entries(object).filter(([key]) => key !== 'agents').map(([key, value]) => <tr key={key}><th scope="row">{key}</th><td>{typeof value === 'string' ? value : JSON.stringify(value)}</td></tr>)}</tbody></table><VirtualList label="Agent dashboard rows" items={agents} render={agent => <span>{agent && typeof agent === 'object' && !Array.isArray(agent) ? Object.entries(agent).map(([key, value]) => key + ': ' + (typeof value === 'string' ? value : JSON.stringify(value))).join(' · ') : String(agent)}</span>} /></section>;
}
export function ExecutionLog({ tab, entries, showStatus, toggleStatus, stopWatch, copy, readOnly = false, minimize }: { tab: TabSnapshot; entries: readonly OutputEntry[]; showStatus: boolean; toggleStatus: () => void; stopWatch: () => void; copy: () => void; readOnly?: boolean; minimize?: () => void }) {
  const lines = useMemo(() => {
    const tail: { text: string; level: string }[] = [];
    for (let index = entries.length - 1; index >= 0 && tail.length < 50_000; index--) {
      const entry = entries[index]; if (!entry) continue;
      const text = outputText(entry).split('\n');
      for (let line = text.length - 1; line >= 0 && tail.length < 50_000; line--) tail.push({ text: text[line] ?? '', level: entry.kind === 'text' ? entry.level : entry.kind === 'finished' && entry.result.error ? 'error' : 'info' });
    }
    return tail.reverse();
  }, [entries]);
  return <div className={`execution phase-${tab.execution_phase.phase}`} data-key-context="execution"><header><h2>Execution</h2><span className="badge">{phaseLabel(tab.execution_phase.phase)}{'command' in tab.execution_phase ? ` · ${tab.execution_phase.command}` : ''}</span>{minimize && <button className="secondary panel-minimize" aria-label="Minimize execution" title="Minimize execution" onClick={minimize}>−</button>}</header>
    <div className="surface-toolbar"><button className="secondary" aria-pressed={showStatus} onClick={toggleStatus}>Status log</button><Menu label="Output actions" actions={[
      { id: 'copy', label: 'Copy selection', group: 'Output', matrix: ['exec.select_copy'], run: copy },
      ...(tab.agents_dashboard?.watching ? [{ id: 'stop', label: 'Stop watching', group: 'Danger' as const, matrix: ['exec.agents_watch_stop'], disabled: readOnly ? 'Attach as primary to stop watching.' : undefined, run: stopWatch }] : []),
    ]} /></div>
    {tab.agents_dashboard && <Dashboard overview={tab.agents_dashboard.overview} />}
    <VirtualList label="Command output" follow rowHeight={28} items={lines} render={line => <span className={`log-line level-${line.level}`}>{line.text || ' '}</span>} />
    {showStatus && <VirtualList label="Status messages" rowHeight={28} items={tab.status_log} render={line => <span className={`log-line level-${line.level}`}>{line.text}</span>} />}
    {tab.last_container_summary && <p className="summary-bar">{tab.last_container_summary.agent_display_name} · {tab.last_container_summary.container_name} · CPU {tab.last_container_summary.avg_cpu} · Memory {tab.last_container_summary.avg_memory} · {tab.last_container_summary.total_time} · exit {tab.last_container_summary.exit_code}</p>}
  </div>;
}
export function GitPanel({ tab, close }: { tab: TabSnapshot; close: () => void }) {
  const git = tab.git_summary;
  return <section className="panel git-panel"><header><h2>Git · {git?.branch ?? 'No branch'}</h2><button className="secondary" onClick={close}>Close Git</button></header><p>{git?.files.length ?? 0} files · +{git?.added ?? 0} / −{git?.removed ?? 0}</p>
    <VirtualList label="Changed files" items={git?.files ?? []} render={f => <span className="git-file"><span className="badge">{f.change}</span><span className="git-path" title={f.path}>{f.path}</span><span>{f.binary ? 'binary' : `+${f.added} −${f.removed}`}</span></span>} />
  </section>;
}
const WINDOWED_STEPS = 60;
export function WorkflowPanel({ tab, close, control, readOnly = false }: { tab: TabSnapshot; close: () => void; control: (step: string) => void; readOnly?: boolean }) {
  const [copied, setCopied] = useState('');
  // undefined follows the current step; null means every row is collapsed.
  const [chosen, setChosen] = useState<string | null | undefined>(undefined);
  const steps = tab.workflow?.steps ?? [];
  const selected = chosen === null ? null : steps.find(s => s.name === (chosen ?? tab.workflow?.current_step)) ?? null;
  // One-line rows; the selected step expands. Ordinary workflows render every
  // row so the inspector is the only scroll region; very long ones are windowed.
  const windowed = steps.length > WINDOWED_STEPS;
  const row = (s: WorkflowStepView) => <button className="secondary workflow-step-row" aria-expanded={selected?.name === s.name} onClick={() => setChosen(selected?.name === s.name ? null : s.name)}>{s.name} · {s.status}{s.agent ? ` · ${s.agent}` : ''}</button>;
  const detail = selected && <div className="workflow-step-detail" role="region" aria-label={`Step ${selected.name}`}><p>{selected.kind} · {selected.agent} {selected.model}</p><p>Depends on: {selected.depends_on.join(', ') || '—'}</p>{tab.workflow?.reported[selected.name] && <p>{tab.workflow.reported[selected.name]}</p>}<button className="secondary" disabled={readOnly} onClick={() => control(selected.name)}>Control step</button></div>;
  return <section className="panel workflow-panel"><header><h2>Workflow overview</h2><button className="secondary" onClick={close}>Close overview</button></header><p className="hint">{tab.current_workflow?.workflow_name} · {tab.current_workflow?.phase} · {tab.workflow?.max_concurrent ?? 1} concurrent</p>
    {tab.context_path && <button className="secondary" onClick={() => void copyText(tab.context_path ?? '').then(() => setCopied('Copied context path')).catch(() => setCopied('Copy failed'))}>Copy context path</button>}<p role="status">{copied}</p>
    {windowed ? <><VirtualList label="Workflow steps" activeIndex={selected ? steps.indexOf(selected) : undefined} items={steps} render={row} />{detail}</>
      : <div className="stack" role="group" aria-label="Workflow steps">{steps.map(s => <Fragment key={s.name}>{row(s)}{selected === s && detail}</Fragment>)}</div>}
    {!steps.length && <p>No workflow is running.</p>}
  </section>;
}
function AcpMessage({ update }: { update: SessionUpdate }) {
  let text: string;
  if (update.sessionUpdate === 'agent_message_chunk' || update.sessionUpdate === 'agent_thought_chunk') {
    const content = update.content;
    text = content.type === 'text' ? content.text : content.type === 'resource_link' ? content.name + ' · ' + content.uri : content.type === 'image' || content.type === 'audio' ? content.type + ' · ' + content.mime_type : JSON.stringify(content.resource, null, 2);
  } else if (update.sessionUpdate === 'plan') text = update.entries.map(entry => entry.status + ' · ' + entry.content).join('\n');
  else if (update.sessionUpdate === 'available_commands_update') text = update.available_commands.map(command => command.name + ' · ' + command.description).join('\n');
  else text = [update.title ?? update.toolCallId, update.status, JSON.stringify(update.content ?? [], null, 2)].filter(Boolean).join('\n');
  return <article className="acp-message"><h3>{update.sessionUpdate.replaceAll('_', ' ')}</h3><pre>{text}</pre></article>;
}
/** Window actions for an agent slot, merged into its one overflow menu. */
function windowActions(slot: SlotSnapshot, setWindow: (id: string, state: 'hidden' | 'minimized' | 'maximized') => void): ShellAction[] {
  return [
    { id: 'collapse', label: 'Collapse agent', group: 'Agent', matrix: ['ctr.window_cycle', 'ctr.detach'], run: () => setWindow(slot.slot_id, 'minimized') },
    { id: 'hide', label: 'Hide agent', group: 'Agent', matrix: ['ctr.window_cycle'], run: () => setWindow(slot.slot_id, 'hidden') },
  ];
}
function slotTitle(slot: SlotSnapshot): string {
  return `${slot.phase_step_title ?? slot.step_name} · ${slot.agent_display_name} · ${slot.container_name}`;
}
/**
 * Every agent terminal of a tab stays mounted (and connected) while it is
 * hidden: switching agent, panel or tab only changes visibility, so control
 * and scrollback survive. Only the visible slot gets a header, which is the
 * terminal's own single row: `lead` (panel switch, agent selector) +
 * identity + control status + one "Agent actions" menu.
 */
export function ContainerArea({ client, readOnly, viewOnly, tab, focused, setFocused, windows, setWindow, interactive = true, hidden = false, lead, overlay }: Pick<PanelContext, 'client' | 'readOnly'> & { tab: TabSnapshot; focused: string | null; setFocused: (id: string) => void; windows: Record<string, string>; interactive?: boolean; setWindow: (id: string, state: 'hidden' | 'minimized' | 'maximized') => void; viewOnly?: boolean; hidden?: boolean; lead?: ReactNode; overlay?: ReactNode }) {
  return <div className="container-area" hidden={hidden}>{tab.slots.map(slot => {
    const state = windows[slot.slot_id] ?? 'maximized'; const active = focused === slot.slot_id || (!focused && tab.slots[0]?.slot_id === slot.slot_id);
    const visible = !hidden && active && state === 'maximized';
    // With several agents the selector in `lead` names the agent; only the status remains here.
    const identity = <span className="agent-identity" title={slotTitle(slot)}>{tab.slots.length > 1 ? `${statusGlyph(slot.program_status)}${slot.stuck ? ' ⚠ Stuck' : ''}` : <SlotBar slot={slot} />}</span>;
    const header = <div className="agent-lead">{lead}{identity}</div>;
    if (slot.terminal_id) {
      return <section className={`container-slot ${active ? 'slot-active' : ''}`} key={slot.slot_id} hidden={!visible} aria-label={`${slot.step_name} agent`}>
        <TerminalPanel client={client} terminalId={slot.terminal_id} title={slotTitle(slot)} focused={visible && interactive} working={slot.program_status?.state === 'working'} readOnly={readOnly} viewOnly={viewOnly ?? readOnly} lead={visible ? header : undefined} windowActions={windowActions(slot, setWindow)} overlay={visible ? overlay : undefined} />
      </section>;
    }
    if (!active) return null;
    return <section className={`container-slot ${active ? 'slot-active' : ''}`} key={slot.slot_id} hidden={!visible} aria-label={`${slot.step_name} agent`}>
      <header className="agent-header">{header}<Menu label="Agent actions" actions={[{ id: 'focus', label: 'Focus agent', group: 'Agent', matrix: ['nav.active_agent_chip'], run: () => setFocused(slot.slot_id) }, ...windowActions(slot, setWindow)]} /></header>
      {overlay && <div className="stage-overlay">{overlay}</div>}
      {slot.acp ? <div className="acp-panel" aria-label="ACP agent"><VirtualList items={slot.acp.history} rowHeight={132} label="Agent messages" render={update => <AcpMessage update={update} />} />{slot.acp.pending_permission && <p role="status">Permission requested: {slot.acp.pending_permission.toolCall.title}. Answer the engine dialog.</p>}</div> : <p>Waiting for terminal…</p>}
    </section>;
  })}</div>;
}
/** Info and success notices expire on their own; warnings and errors stay until dismissed. */
export const NOTICE_EXPIRY_MS = 8_000;
export function NoticeExpiry({ notices, dismiss }: { notices: readonly { id: number; level: string }[]; dismiss: (id: number) => void }) {
  const timers = useRef(new Map<number, ReturnType<typeof setTimeout>>());
  const dismissRef = useRef(dismiss);
  useEffect(() => { dismissRef.current = dismiss; });
  const expiring = notices.filter(n => n.level === 'info' || n.level === 'success').map(n => n.id).join(',');
  useEffect(() => {
    const live = new Set(expiring ? expiring.split(',').map(Number) : []);
    for (const [id, timer] of timers.current) if (!live.has(id)) { clearTimeout(timer); timers.current.delete(id); }
    for (const id of live) if (!timers.current.has(id)) timers.current.set(id, setTimeout(() => { timers.current.delete(id); dismissRef.current(id); }, NOTICE_EXPIRY_MS));
  }, [expiring]);
  useEffect(() => { const pending = timers.current; return () => { for (const timer of pending.values()) clearTimeout(timer); pending.clear(); }; }, []);
  return null;
}
/** Desktop Agent / Output switch; part of the agent header or the stage heading. */
export function StageSwitch({ agent, agentDisabled, showAgent, showOutput }: { agent: boolean; agentDisabled: boolean; showAgent: () => void; showOutput: () => void }) {
  return <div className="cluster stage-switch" role="group" aria-label="Stage"><button className="secondary" aria-pressed={agent} disabled={agentDisabled} onClick={showAgent}>Agent</button><button className="secondary" aria-pressed={!agent} onClick={showOutput}>Output</button></div>;
}
export function AgentSelector({ slots, value, choose }: { slots: readonly SlotSnapshot[]; value: string; choose: (id: string) => void }) {
  return <label className="agent-selector"><span className="sr-only">Active agent</span><select aria-label="Active agent" value={value} onChange={e => choose(e.target.value)}>{slots.map(slot => <option key={slot.slot_id} value={slot.slot_id}>{slot.phase_step_title ?? slot.step_name} · {slot.agent_display_name}</option>)}</select></label>;
}
/** Re-renders its children when a media query changes (e.g. narrow ↔ wide on resize). */
export function MediaQuery({ query, children }: { query: string; children: (matches: boolean) => ReactNode }) {
  const matches = useSyncExternalStore(
    useCallback(notify => { if (typeof matchMedia !== 'function') return () => undefined; const list = matchMedia(query); list.addEventListener?.('change', notify); return () => list.removeEventListener?.('change', notify); }, [query]),
    () => typeof matchMedia === 'function' && matchMedia(query).matches,
    () => false,
  );
  return <>{children(matches)}</>;
}
