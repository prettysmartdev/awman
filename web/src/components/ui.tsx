'use client';
import { createContext, useContext, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { isTextInput, useShortcuts } from '@/lib/keys/useShortcuts';
import { chordOf, isReservedChord } from '@/lib/keys/keymap';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { Observable } from '@/lib/client/observable';
import { useSyncExternalStore } from 'react';

export function useObservable<T>(value: Observable<T>): T {
  return useSyncExternalStore(cb => value.subscribe(cb), () => value.get(), () => value.get());
}
export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : 'The action could not be completed.';
}
export async function copyText(text: string): Promise<void> { await navigator.clipboard.writeText(text); }

/** Presentation adapter for existing Queue and countdown content. No answers
 * are emitted by this context or by changing panels. */
export const SheetPresentation = createContext<'modal' | 'drawer' | 'banner'>('modal');

export function Modal({ title, children, onDismiss, initialFocus }: { title: string; children: ReactNode; onDismiss?: () => void; initialFocus?: string }) {
  const presentation = useContext(SheetPresentation);
  const ref = useRef<HTMLDivElement>(null);
  // An embedded drawer shares Escape with a live countdown banner: the drawer
  // takes it only while focus is inside the drawer (decided by focus).
  const sheet = useRef<HTMLElement>(null);
  useScreenShortcuts(() => ({ context: 'dialog', opts: { dialog: 'other' } }), action => { if (action === 'dialog.dismiss' && onDismiss && (presentation === 'drawer' ? !document.querySelector('[aria-modal=true]') && (!document.querySelector('.embedded-banner') || !!sheet.current?.contains(document.activeElement)) :[...document.querySelectorAll('[role=dialog]')].at(-1) === ref.current)) { onDismiss(); return; } return false; }, !!onDismiss);
  useEffect(() => {
    if (presentation !== 'modal') return;
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const node = ref.current;
    (node?.querySelector<HTMLElement>(initialFocus ?? 'input:not(:disabled),textarea:not(:disabled),button:not(:disabled),[tabindex="0"]') ?? node)?.focus();
    const trap = (event: KeyboardEvent) => {
      if (event.key !== 'Tab' || event.ctrlKey || event.altKey || event.metaKey) return;
      const nodes = [...(node?.querySelectorAll<HTMLElement>('button:not(:disabled),input:not(:disabled),textarea:not(:disabled),select,[tabindex="0"],a[href]') ?? [])];
      const first = nodes[0]; const last = nodes[nodes.length - 1];
      if (!first) { event.preventDefault(); return; }
      if (event.shiftKey && (document.activeElement === first || document.activeElement === node)) { last?.focus(); event.preventDefault(); }
      if (!event.shiftKey && document.activeElement === last) { first.focus(); event.preventDefault(); }
    };
    node?.addEventListener('keydown', trap);
    return () => { node?.removeEventListener('keydown', trap); previous?.focus(); };
  }, [presentation, initialFocus]);
  if (presentation !== 'modal') return <section className={`embedded-sheet embedded-${presentation}`} aria-label={title} ref={sheet}><header><h2>{title}</h2>{onDismiss && <button className="secondary" onClick={onDismiss}>Close</button>}</header>{children}</section>;
  return <div className="modal-backdrop"><div className="modal" role="dialog" aria-modal="true" aria-label={title} tabIndex={-1} ref={ref}>
    <header><h2>{title}</h2>{onDismiss && <button className="secondary" onClick={onDismiss}>Close</button>}</header>{children}
  </div></div>;
}

/** Fixed-height window: only the visible rows + overscan enter the DOM. SVG
 * spacers use intrinsic dimensions, so CSP never needs inline CSS. */
export function VirtualList<T>({ items, render, label, rowHeight = 44, activeIndex, follow = false }: { items: readonly T[]; render: (item: T, index: number) => ReactNode; label: string; rowHeight?: number; activeIndex?: number; follow?: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const [scroll, setScroll] = useState(0);
  const following = useRef(follow);
  useLayoutEffect(() => {
    const node = ref.current; if (!node) return;
    if (follow && following.current) { node.scrollTop = Math.max(0, items.length * rowHeight - node.clientHeight); setScroll(node.scrollTop); }
  }, [follow, items.length, rowHeight]);
  useLayoutEffect(() => {
    const node = ref.current; if (!node || activeIndex === undefined) return;
    const top = activeIndex * rowHeight;
    if (top < node.scrollTop || top + rowHeight > node.scrollTop + node.clientHeight) { node.scrollTop = top; setScroll(top); }
  }, [activeIndex, rowHeight]);
  const start = Math.min(Math.max(0, Math.floor(scroll / rowHeight) - 5), Math.max(0, items.length - 30));
  const end = Math.min(items.length, start + 40);
  return <div className="virtual-frame"><div className="list-tools"><span>{items.length} entries</span><button className="secondary" aria-label={`Top of ${label}`} onClick={() => { if (ref.current) ref.current.scrollTop = 0; }}>↑</button><button className="secondary" aria-label={`End of ${label}`} onClick={() => { if (ref.current) ref.current.scrollTop = items.length * rowHeight; }}>↓</button></div>
    <div className={`virtual-list rows-${rowHeight}`} role="region" aria-label={label} tabIndex={0} ref={ref} onScroll={e => { const node = e.currentTarget; following.current = node.scrollHeight - node.scrollTop - node.clientHeight < rowHeight * 2; setScroll(node.scrollTop); }}>
      <svg aria-hidden="true" width="1" height={start * rowHeight} className="virtual-spacer" />
      {items.slice(start, end).map((item, index) => <div className="virtual-row" key={start + index}>{render(item, start + index)}</div>)}
      <svg aria-hidden="true" width="1" height={Math.max(0, (items.length - end) * rowHeight)} className="virtual-spacer" />
    </div></div>;
}

/** Consume reserved-chord metadata from the single map before dispatching UI
 * actions. Native selection copy inside editors stays a browser action. */
export function useScreenShortcuts(scope: Parameters<typeof useShortcuts>[0], action: Parameters<typeof useShortcuts>[1], enabled = true) {
  useShortcuts(scope, (mapped, event) => {
    const chord = chordOf(event);
    const text = `${chord.ctrl ? 'Ctrl+' : ''}${chord.alt ? 'Alt+' : ''}${chord.shift ? 'Shift+' : ''}${chord.key.length === 1 ? chord.key.toUpperCase() : chord.key}`;
    if (isReservedChord(text)) return false;
    const element = event.target;
    if (isTextInput(element) && (mapped === 'workflow.overviewLeft' || mapped === 'workflow.overviewRight')) return false;
    if (mapped === 'interrupt' && (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) && element.selectionStart !== element.selectionEnd) return false;
    return action(mapped, event);
  }, enabled);
}

export function selectedText(): string {
  const element = document.activeElement;
  if ((element instanceof HTMLTextAreaElement || (element instanceof HTMLInputElement && element.type !== 'password')) && element.selectionStart !== null && element.selectionEnd !== null) return element.value.slice(element.selectionStart, element.selectionEnd);
  return window.getSelection()?.toString() ?? '';
}

/** Named UI actions are projected by the engine catalogue. Task names remain
 * individual argv entries; they never become shell source. */
export async function actionArgs(client: DaemonClient, action: string, name?: string): Promise<string[]> {
  const args = (await client.catalogue()).actions[action];
  if (!args?.length) throw new Error(`The engine catalogue does not offer ${action}.`);
  const result = [...args];
  if (name !== undefined) { const flag = result.findIndex(arg => arg.startsWith('-')); result.splice(flag < 0 ? result.length : flag, 0, name); }
  return result;
}
