'use client';
import { useState } from 'react';
import { Modal } from '@/components/ui';
import { ACTION_GROUPS, invokeAction, type ShellAction } from './actions';

export function CommandMenu({ actions, close }: { actions: readonly ShellAction[]; close(): void }) {
  const [query, setQuery] = useState('');
  const visible = actions.filter(a => a.menu !== false && `${a.label} ${a.group} ${a.description ?? ''}`.toLowerCase().includes(query.toLowerCase()));
  function choose(action: ShellAction) {
    if (action.disabled) return;
    close();
    // Let the sheet restore its opener before an action moves focus elsewhere.
    requestAnimationFrame(() => invokeAction(action));
  }
  return <Modal title="Actions" onDismiss={close} initialFocus="#action-search"><div className="command-menu" onKeyDown={event => {
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    if ((event.key === 'Home' || event.key === 'End') && event.target instanceof HTMLInputElement) return;
    const buttons = [...event.currentTarget.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')];
    if (!buttons.length) return;
    const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (at + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
    buttons[next]?.focus(); event.preventDefault();
  }}><label className="sr-only" htmlFor="action-search">Find an action or command</label><input id="action-search" type="search" placeholder="Find an action or command…" value={query} onChange={e => setQuery(e.target.value)} />
    <div className="command-results">{ACTION_GROUPS.map(group => {
      const entries = visible.filter(a => a.group === group);
      return entries.length ? <section key={group} aria-label={group}><h3>{group}</h3>{entries.map(action => <div className="command-option" key={action.id}><button data-shortcut={action.shortcut} className={`secondary ${group === 'Danger' ? 'danger' : ''}`} disabled={!!action.disabled} title={action.disabled} onClick={() => choose(action)}>{action.label}</button>{(action.disabled || action.description) && <p className="hint">{action.disabled ?? action.description}</p>}</div>)}</section> : null;
    })}{!visible.length && <p>No matching actions.</p>}</div>
  </div></Modal>;
}
