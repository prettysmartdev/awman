'use client';
import { useEffect, useId, useRef, useState } from 'react';
import { invokeAction, type ShellAction } from './actions';

/** Context menus share pointer, keyboard, dismissal and focus-return behavior.
 * A disabled reason sits beside its item, not inside it, so it keeps full contrast. */
export function Menu({ label, actions }: { label: string; actions: readonly ShellAction[] }) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null); const trigger = useRef<HTMLButtonElement>(null); const id = useId();
  useEffect(() => {
    if (!open) return;
    root.current?.querySelector<HTMLButtonElement>('[role=menuitem]:not(:disabled)')?.focus();
    const outside = (event: PointerEvent) => { if (!root.current?.contains(event.target as Node)) setOpen(false); };
    document.addEventListener('pointerdown', outside);
    return () => document.removeEventListener('pointerdown', outside);
  }, [open]);
  return <div className="context-menu" ref={root} onBlur={e => { if (!e.currentTarget.contains(e.relatedTarget)) setOpen(false); }} onKeyDown={e => {
    if (!open) return;
    if (e.key === 'Escape') { setOpen(false); trigger.current?.focus(); e.stopPropagation(); e.preventDefault(); }
    if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) {
      const items = [...(root.current?.querySelectorAll<HTMLButtonElement>('[role=menuitem]:not(:disabled)') ?? [])];
      const at = items.indexOf(document.activeElement as HTMLButtonElement);
      items[e.key === 'Home' ? 0 : e.key === 'End' ? items.length - 1 : (at + (e.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length]?.focus(); e.preventDefault();
    }
  }}><button ref={trigger} className="secondary" aria-haspopup="menu" aria-expanded={open} aria-controls={open ? id : undefined} onClick={() => setOpen(v => !v)}>{label}</button>{open && <div id={id} className="menu-popover" role="menu" aria-label={label}>{actions.map((action, index) => <div role="none" className="command-option" key={action.id}><button role="menuitem" className={`secondary ${action.group === 'Danger' ? 'danger' : ''}`} disabled={!!action.disabled} aria-describedby={action.disabled ? `${id}-reason-${index}` : undefined} onClick={() => { setOpen(false); trigger.current?.focus(); invokeAction(action); }}>{action.label}</button>{action.disabled && <small id={`${id}-reason-${index}`}>{action.disabled}</small>}</div>)}</div>}</div>;
}
