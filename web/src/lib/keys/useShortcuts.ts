'use client';

// Document-level keyboard dispatch through the one keymap (design §5.6).
// The caller supplies the current focus context; a handled action prevents
// the browser default. Keys typed into a focused text field are left alone
// unless the keymap claims them for that context.
import { useEffect, useRef } from 'react';
import { map, type KeyContext, type MapOptions, type WebAction } from './keymap';

export interface ShortcutScope {
  context: KeyContext;
  opts?: MapOptions;
}

export function isTextInput(target: EventTarget | null): boolean {
  if (!target || typeof (target as HTMLElement).tagName !== 'string') return false;
  const element = target as HTMLElement;
  if (element.isContentEditable) return true;
  if (element.tagName === 'TEXTAREA') return true;
  if (element.tagName !== 'INPUT') return false;
  const type = (element as HTMLInputElement).type;
  return !['button', 'checkbox', 'radio', 'submit', 'reset', 'range', 'color', 'file'].includes(type);
}

/**
 * `scope()` is read on every key press (so it always reflects current focus);
 * `onAction` returns false to decline an action (the browser default then runs).
 */
export function useShortcuts(scope: () => ShortcutScope, onAction: (action: WebAction, event: KeyboardEvent) => boolean | undefined, enabled = true): void {
  const scopeRef = useRef(scope);
  const actionRef = useRef(onAction);
  useEffect(() => {
    scopeRef.current = scope;
    actionRef.current = onAction;
  });
  useEffect(() => {
    if (!enabled) return;
    const listener = (event: KeyboardEvent) => {
      if (event.defaultPrevented) return;
      const { context, opts = {} } = scopeRef.current();
      const action = map(event, context, { ...opts, textInput: opts.textInput ?? isTextInput(event.target) });
      if (!action || action === 'terminal.forward') return; // the focused terminal encodes these itself
      if (actionRef.current(action, event) === false) return;
      event.preventDefault();
      event.stopPropagation();
    };
    document.addEventListener('keydown', listener);
    return () => document.removeEventListener('keydown', listener);
  }, [enabled]);
}
