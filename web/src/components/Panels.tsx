'use client';
import { useMemo, useRef, useState } from 'react';
import type { OutputEntry } from '@/lib/client/instanceStore';
import type { EffectiveStatus, JsonValue, SlotSnapshot, TabSnapshot } from '@/lib/client/types';
import type { PanelContext } from '@/lib/shell/panels';
import { countdownPresentation } from './presentation';
import { TerminalPanel } from './TerminalPanel';
import { copyText, VirtualList } from './ui';

export function statusGlyph(s: EffectiveStatus | null | undefined): string {
  if (!s) return '';
  return `${s.state === 'working' ? '◐' : s.state === 'done' ? '✓' : s.state === 'error' ? '✗' : s.state === 'blocked' ? s.kind === 'question' ? '?' : '🔒' : '○'}${s.progress !== null ? ` ${s.progress}%` : ''} ${s.msg ?? ''}`;
}
export function SlotBar({ slot }: { slot: SlotSnapshot }) {
  return <><strong>{slot.phase_step_title ?? slot.step_name} · {slot.agent_display_name}</strong> {slot.model} · {slot.container_name} {statusGlyph(slot.program_status)} {slot.stuck ? '⚠ Stuck' : ''} {slot.yolo ? countdownPresentation(slot.yolo.kind, slot.yolo.remaining_secs).title : ''} {slot.stats ? `CPU ${slot.stats.cpu_percent}% · ${slot.stats.memory_mb} MB` : ''}{slot.dormant ? ' · dormant' : ''}</>;
}
export function TabBar({ tabs, active, select, close, newTab, readOnly }: { tabs: TabSnapshot[]; active: string | null; select: (id: string) => void; close: (id: string) => void; newTab: () => void; readOnly: boolean }) {
  const touch = useRef<number | null>(null);
  return <div className="tab-strip" onTouchStart={e => { touch.current = e.changedTouches[0]?.clientX ?? null; }} onTouchEnd={e => {
    const x = e.changedTouches[0]?.clientX; if (x === undefined || touch.current === null || Math.abs(x - touch.current) < 60) return;
    const i = tabs.findIndex(t => t.tab_id === active); const next = tabs[(i + (x > touch.current ? 1 : -1) + tabs.length) % tabs.length]; if (next) select(next.tab_id);
  }}><div role="tablist" aria-label="Workspace tabs">{tabs.map(t => <div className={`tab-item label-${t.label.color} ${t.kind === 'squad' ? 'squad-tab' : ''}`} key={t.tab_id}>
    <button role="tab" aria-selected={active === t.tab_id} title={`${t.label.text} · ${t.execution_phase.phase}${'command' in t.execution_phase ? `: ${t.execution_phase.command}` : ''}`} onClick={() => select(t.tab_id)}>{active === t.tab_id ? '➡ ' : ''}{t.label.text} {t.stuck ? '⚠' : ''} {statusGlyph(t.slots.find(s => s.slot_id === t.focused_slot)?.program_status ?? t.slots[0]?.program_status)}{t.yolo ? ` ${countdownPresentation(t.yolo.kind, t.yolo.remaining_secs).title}` : ''}</button>
    <button className="secondary" aria-label={`Close ${t.label.text}`} disabled={readOnly} onClick={() => close(t.tab_id)}>×</button>
  </div>)}</div><button aria-label="New tab" disabled={readOnly} onClick={newTab}>+</button></div>;
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
export function ExecutionLog({ tab, entries, showStatus, toggleStatus, stopWatch, copy }: { tab: TabSnapshot; entries: readonly OutputEntry[]; showStatus: boolean; toggleStatus: () => void; stopWatch: () => void; copy: () => void }) {
  const lines = useMemo(() => {
    const tail: { text: string; level: string }[] = [];
    for (let index = entries.length - 1; index >= 0 && tail.length < 50_000; index--) {
      const entry = entries[index]; if (!entry) continue;
      const text = outputText(entry).split('\n');
      for (let line = text.length - 1; line >= 0 && tail.length < 50_000; line--) tail.push({ text: text[line] ?? '', level: entry.kind === 'text' ? entry.level : entry.kind === 'finished' && entry.result.error ? 'error' : 'info' });
    }
    return tail.reverse();
  }, [entries]);
  return <div className={`execution phase-${tab.execution_phase.phase}`} data-key-context="execution"><header><h2>Execution</h2><span className="badge">{tab.execution_phase.phase}{'command' in tab.execution_phase ? ` · ${tab.execution_phase.command}` : ''}</span></header>
    <div className="actions"><button className="secondary" aria-expanded={showStatus} onClick={toggleStatus}>Status log</button><button className="secondary" onPointerDown={e => e.preventDefault()} onClick={copy}>Copy</button>{tab.agents_dashboard?.watching && <button onClick={stopWatch}>Stop watching</button>}</div>
    {tab.agents_dashboard && <Dashboard overview={tab.agents_dashboard.overview} />}
    <VirtualList label="Command output" follow rowHeight={28} items={lines} render={line => <span className={`log-line level-${line.level}`}>{line.text || ' '}</span>} />
    {showStatus && <VirtualList label="Status messages" rowHeight={28} items={tab.status_log} render={line => <span className={`log-line level-${line.level}`}>{line.text}</span>} />}
    {tab.last_container_summary && <p className="summary-bar">{tab.last_container_summary.agent_display_name} · {tab.last_container_summary.container_name} · CPU {tab.last_container_summary.avg_cpu} · Memory {tab.last_container_summary.avg_memory} · {tab.last_container_summary.total_time} · exit {tab.last_container_summary.exit_code}</p>}
  </div>;
}
export function GitPanel({ tab, close }: { tab: TabSnapshot; close: () => void }) {
  const git = tab.git_summary;
  return <section className="panel"><header><h2>Git · {git?.branch ?? 'No branch'}</h2><button className="secondary" onClick={close}>Close Git</button></header><p>{git?.files.length ?? 0} files · +{git?.added ?? 0} / −{git?.removed ?? 0}</p>
    <VirtualList label="Changed files" items={git?.files ?? []} render={f => <span title={f.path}>{f.change} · {f.path} · {f.binary ? 'binary' : `+${f.added} −${f.removed}`}</span>} />
  </section>;
}
export function WorkflowPanel({ tab, close, control }: { tab: TabSnapshot; close: () => void; control: (step: string) => void }) {
  const ref = useRef<HTMLDivElement>(null); const [copied, setCopied] = useState('');
  const [left, setLeft] = useState(0);
  const steps = tab.workflow?.steps ?? []; const start = Math.max(0, Math.min(Math.floor(left / 232) - 3, steps.length - 20)); const end = Math.min(steps.length, start + 20);
  return <section className="panel"><header><h2>Workflow overview</h2><button className="secondary" onClick={close}>Close overview</button></header><p>{tab.current_workflow?.workflow_name} · {tab.current_workflow?.phase} · {tab.workflow?.max_concurrent ?? 1} concurrent</p><div className="actions"><button className="secondary" aria-label="Scroll workflow left" onClick={() => ref.current?.scrollBy({ left: -240 })}>‹</button><button className="secondary" aria-label="Scroll workflow right" onClick={() => ref.current?.scrollBy({ left: 240 })}>›</button>{tab.context_path && <button onClick={() => void copyText(tab.context_path ?? '').then(() => setCopied('Copied context path')).catch(() => setCopied('Copy failed'))}>Copy context path</button>}</div><p role="status">{copied}</p>
    <div className="workflow-scroll" ref={ref} tabIndex={0} aria-label="Workflow steps" onScroll={event => setLeft(event.currentTarget.scrollLeft)}><svg className="virtual-spacer" aria-hidden="true" width={Math.max(0, start * 232 - 12)} height="1" />{steps.slice(start, end).map(s => <article className={`workflow-step phase-${s.status}`} key={s.name}><h3>{s.name}</h3><p>{s.status} · {s.kind}</p><p>{s.agent} {s.model}</p><p>Depends on: {s.depends_on.join(', ') || '—'}</p><p>{tab.workflow?.reported[s.name]}</p><button className="secondary" onClick={() => control(s.name)}>Workflow</button></article>)}<svg className="virtual-spacer" aria-hidden="true" width={Math.max(0, (steps.length - end) * 232 - 12)} height="1" />{!steps.length && <p>No workflow is running.</p>}</div>
  </section>;
}
export function ContainerArea({ client, readOnly, tab, focused, setFocused, windows, setWindow, interactive = true }: Pick<PanelContext, 'client' | 'readOnly'> & { tab: TabSnapshot; focused: string | null; setFocused: (id: string) => void; windows: Record<string, string>; interactive?: boolean; setWindow: (id: string, state: 'hidden' | 'minimized' | 'maximized') => void }) {
  return <div className="container-area">{tab.slots.map(slot => {
    const state = windows[slot.slot_id] ?? 'maximized'; const active = focused === slot.slot_id || (!focused && tab.slots[0]?.slot_id === slot.slot_id);
    return <section className={`container-slot ${active ? 'slot-active' : ''}`} key={slot.slot_id}><header><button className="secondary slot-label" aria-pressed={active} onClick={() => setFocused(slot.slot_id)}><SlotBar slot={slot} /></button><button className="secondary" aria-label={`Show ${slot.step_name}`} onClick={() => { setFocused(slot.slot_id); setWindow(slot.slot_id, 'maximized'); }}>Show</button><button className="secondary" aria-label={`Minimize ${slot.step_name}`} onClick={() => setWindow(slot.slot_id, 'minimized')}>Minimize</button><button className="secondary" aria-label={`Hide ${slot.step_name}`} onClick={() => setWindow(slot.slot_id, 'hidden')}>Hide</button></header>
      {active && state === 'maximized' && (slot.terminal_id ? <TerminalPanel client={client} terminalId={slot.terminal_id} title={`${slot.phase_step_title ?? slot.step_name} · ${slot.agent_display_name} · ${slot.container_name}`} focused={active && interactive} working={slot.program_status?.state === 'working'} readOnly={readOnly} /> : slot.acp ? <div className="acp-panel" aria-label="ACP agent"><VirtualList items={slot.acp.history} label="Agent messages" render={update => <span>{JSON.stringify(update)}</span>} />{slot.acp.pending_permission && <p role="status">Permission requested: {slot.acp.pending_permission.toolCall.title}. Answer the engine dialog.</p>}</div> : <p>Waiting for terminal…</p>)}
    </section>;
  })}</div>;
}
