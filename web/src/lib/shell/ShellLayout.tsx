'use client';

// Responsive workspace skeleton (design §5.9). Desktop: tab bar, status row,
// execution area with optional container/git/workflow panels, composer.
// Narrow (<768 px): one panel at a time chosen from a bottom navigation
// (Log / Agent / Workflow / Git / More) with a persistent active-agent chip.
// Layout only: all content arrives through `slots` from the screens lane.
import type { NarrowPanel, ShellSlots } from './panels';

export interface ShellLayoutProps {
  slots: ShellSlots;
  gitOpen: boolean;
  workflowOpen: boolean;
  narrowPanel: NarrowPanel;
  onNarrowPanelChange(panel: NarrowPanel): void;
}

const NARROW_ITEMS: { id: NarrowPanel; label: string }[] = [
  { id: 'log', label: 'Log' },
  { id: 'agent', label: 'Agent' },
  { id: 'workflow', label: 'Workflow' },
  { id: 'git', label: 'Git' },
  { id: 'more', label: 'More' },
];

export function ShellLayout({ slots, gitOpen, workflowOpen, narrowPanel, onNarrowPanelChange }: ShellLayoutProps) {
  return (
    <div className="shell" data-narrow-panel={narrowPanel}>
      <header className="shell-tabs">{slots.tabBar}</header>
      <div className="shell-status" role="status">
        {slots.status}
      </div>
      {slots.activeAgent ? <div className="shell-agent-chip">{slots.activeAgent}</div> : null}
      <div className="shell-body">
        {workflowOpen && slots.workflow ? (
          <aside className="shell-workflow" data-panel="workflow" aria-label="Workflow overview">
            {slots.workflow}
          </aside>
        ) : null}
        <main className="shell-main">
          {slots.containers ? (
            <section className="shell-containers" data-panel="agent" aria-label="Agents">
              {slots.containers}
            </section>
          ) : null}
          <section className="shell-execution" data-panel="log" aria-label="Execution log">
            {slots.execution}
          </section>
        </main>
        {gitOpen && slots.git ? (
          <aside className="shell-git" data-panel="git" aria-label="Git changes">
            {slots.git}
          </aside>
        ) : null}
        <section className="shell-more" data-panel="more" aria-label="More">
          {slots.shortcuts}
        </section>
      </div>
      <footer className="shell-composer">{slots.composer}</footer>
      <nav className="shell-narrow-nav" aria-label="Panels">
        {NARROW_ITEMS.map(item => (
          <button key={item.id} type="button" aria-pressed={narrowPanel === item.id} onClick={() => onNarrowPanelChange(item.id)}>
            {item.label}
          </button>
        ))}
      </nav>
      {slots.dialogs}
      <div className="shell-notices" aria-live="polite">
        {slots.notices}
      </div>
    </div>
  );
}
