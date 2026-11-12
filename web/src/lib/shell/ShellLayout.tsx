'use client';
import { useEffect, useRef } from 'react';
import type { InspectorKind, NarrowPanel, ShellSlots } from './panels';

export interface ShellLayoutProps {
  slots: ShellSlots;
  inspector: InspectorKind;
  narrowPanel: NarrowPanel;
  onNarrowPanelChange(panel: NarrowPanel): void;
  onInspectorClose(): void;
  disabledPanels?: Partial<Record<NarrowPanel, string>>;
  attention?: Partial<Record<NarrowPanel, boolean>>;
  /**
   * A nonmodal engine banner (the yolo countdown) is live. Escape is then
   * decided by focus: it closes the inspector only while focus is inside it.
   */
  escapeNeedsFocus?: boolean;
}
/** The narrow layout shows one main panel at a time (shell.css). */
export const NARROW_LAYOUT_QUERY = '(max-width: 767px)';
export function isNarrowLayout(): boolean {
  return typeof matchMedia === 'function' && matchMedia(NARROW_LAYOUT_QUERY).matches;
}
const ITEMS: { id: NarrowPanel; label: string }[] = [
  { id: 'agent', label: 'Agent' }, { id: 'output', label: 'Output' },
  { id: 'workflow', label: 'Workflow' }, { id: 'git', label: 'Git' }, { id: 'more', label: 'More' },
];
export function ShellLayout({ slots, inspector, narrowPanel, onNarrowPanelChange, onInspectorClose, attention, disabledPanels, escapeNeedsFocus = false }: ShellLayoutProps) {
  const drawer = useRef<HTMLElement>(null);
  const close = useRef(onInspectorClose); close.current = onInspectorClose;
  const needsFocus = useRef(escapeNeedsFocus); needsFocus.current = escapeNeedsFocus;
  useEffect(() => {
    if (!inspector) return;
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    // Nonmodal drawer: no focus trap; the stage and composer stay reachable.
    const node = drawer.current; node?.focus();
    const escape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || event.defaultPrevented || document.querySelector('[aria-modal=true]')) return;
      // Queue owns its lease-aware dismissal; its embedded Modal handles Esc.
      if (inspector === 'queue') return;
      if (needsFocus.current && !node?.contains(document.activeElement)) return;
      close.current(); event.preventDefault();
    };
    document.addEventListener('keydown', escape);
    return () => { document.removeEventListener('keydown', escape); if (previous?.isConnected && (document.activeElement === document.body || node?.contains(document.activeElement))) previous.focus(); };
  }, [inspector]);
  return <div className="shell" data-narrow-panel={narrowPanel} data-inspector={inspector ?? 'none'}>
    <header className="shell-app-bar glass">{slots.appBar}</header>
    <div className="shell-context-bar glass">{slots.contextBar}</div>
    <div className="shell-agent-chip glass">{slots.activeAgent}</div>
    <div className="shell-body">
      <main className="shell-stage" aria-label="Workspace stage">{slots.stage}</main>
      {inspector && <aside className="shell-inspector" aria-label={`${inspector} inspector`} tabIndex={-1} ref={drawer}>
        {slots.inspector}
      </aside>}
      <section className="shell-more" aria-label="More workspace actions">{slots.more}</section>
    </div>
    <footer className="shell-composer">{slots.composer}</footer>
    <nav className="shell-narrow-nav glass" aria-label="Panels">{ITEMS.map(item => <button key={item.id} type="button" disabled={!!disabledPanels?.[item.id]} title={disabledPanels?.[item.id]} aria-pressed={narrowPanel === item.id} onClick={() => onNarrowPanelChange(item.id)}>{item.label}{attention?.[item.id] && <span aria-label="Needs attention"> ●</span>}</button>)}</nav>
    {slots.dialogs}
    <div className="shell-notices" aria-live="polite">{slots.notices}</div>
  </div>;
}
