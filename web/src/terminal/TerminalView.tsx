'use client';

// Browser terminal for agent TUIs (design §5.5). Renders one hub terminal
// through a TerminalSession (wterm Ghostty core + CSP-safe DOM renderer).
// The view never opens sockets: the typed client's TerminalConnection owns
// awman.terminal.v1, tickets, grants, reconnect and snapshot recovery.
import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState, useSyncExternalStore, type ReactNode } from 'react';
import { map } from '@/lib/keys/keymap';
import type { TerminalConnectionState } from '@/lib/client/terminalSocket';
import type { TerminalViewHandle, TerminalViewProps } from './contract';
import { clampScrollback, loadGhosttyCore, type CoreFactory } from './core/loadCore';
import { TerminalSession } from './adapter/session';
import { TouchToolbar } from './input/TouchToolbar';
import type { KeyLike } from './input/encoder';
import styles from './TerminalView.module.css';

const WHEEL_LINES = 3;

function statusText(state: TerminalConnectionState, holder: 'me' | 'other' | 'none', inputAllowed: boolean): string {
  if (state === 'connecting') return 'Connecting…';
  if (state === 'reconnecting') return 'Reconnecting…';
  if (state === 'closed') return 'Disconnected';
  if (state === 'exited') return 'Exited';
  if (!inputAllowed) return 'Read-only';
  if (holder === 'me') return 'You have control';
  if (holder === 'other') return 'Someone else has control';
  return 'Viewing — take control to type';
}

function useObservableValue<T>(observable: { get(): T; subscribe(cb: (v: T) => void): () => void }): T {
  return useSyncExternalStore(
    useCallback(cb => observable.subscribe(() => cb()), [observable]),
    () => observable.get(),
    () => observable.get(),
  );
}

/** Build the view around a core factory (tests inject one that loads the WASM from disk). */
export function createTerminalView(coreFactory: CoreFactory) {
  return forwardRef<TerminalViewHandle, TerminalViewProps>(function TerminalView(props, ref) {
    const { connection, title, focused, control, onRequestControl, onLinkDetected, onTitleChange, scrollbackLines, touch, reducedMotion, theme } = props;
    const viewportRef = useRef<HTMLDivElement>(null);
    const screenRef = useRef<HTMLDivElement>(null);
    const measureRef = useRef<HTMLSpanElement>(null);
    const inputRef = useRef<HTMLTextAreaElement>(null);
    const sessionRef = useRef<TerminalSession | null>(null);
    const callbacksRef = useRef({ onLinkDetected, onTitleChange, onRequestControl });
    const [notice, setNotice] = useState('');
    const [exitCode, setExitCode] = useState<number | null>(null);
    const [ready, setReady] = useState(false);
    const [ctrlLatched, setCtrlLatched] = useState(false);
    const [scrolledBack, setScrolledBack] = useState(0);
    const state = useObservableValue(connection.state);
    const { holder, inputAllowed } = control;
    const holding = holder === 'me';

    useEffect(() => {
      callbacksRef.current = { onLinkDetected, onTitleChange, onRequestControl };
    });

    // One session per connection; subscribe synchronously so no frame is missed.
    useEffect(() => {
      const screen = screenRef.current;
      if (!screen) return;
      const session = new TerminalSession(
        connection,
        screen,
        { coreFactory, scrollbackLines: clampScrollback(scrollbackLines), theme },
        {
          onTitle: text => callbacksRef.current.onTitleChange?.(text),
          onLinks: links => callbacksRef.current.onLinkDetected(links),
          onNotice: setNotice,
          onChange: () => setReady(session.ready),
        },
      );
      sessionRef.current = session;
      void session.start();
      const offExit = connection.onExit(code => setExitCode(code));
      const offError = connection.onError(error => setNotice(error.message));
      return () => {
        offExit();
        offError();
        session.dispose();
        if (sessionRef.current === session) sessionRef.current = null;
      };
      // Theme and scrollback are applied below without recreating the session.
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [connection]);

    useEffect(() => sessionRef.current?.setTheme(theme), [theme]);
    useEffect(() => sessionRef.current?.setScrollbackLines(scrollbackLines), [scrollbackLines]);

    // Resize the PTY only while holding the input grant; the connection debounces (150 ms).
    const fit = useCallback(() => {
      const session = sessionRef.current;
      const viewport = viewportRef.current;
      const measure = measureRef.current;
      if (!session?.ready || !session.holding || !viewport || !measure) return;
      const cell = measure.getBoundingClientRect();
      const cellWidth = cell.width / 10;
      if (cellWidth <= 0 || cell.height <= 0) return;
      const cols = Math.max(2, Math.floor(viewport.clientWidth / cellWidth));
      const rows = Math.max(2, Math.floor(viewport.clientHeight / cell.height));
      const current = session.gridSize();
      if (cols !== current.cols || rows !== current.rows) connection.resize(cols, rows);
    }, [connection]);

    useEffect(() => {
      if (holding && ready) fit();
    }, [holding, ready, fit]);

    useEffect(() => {
      const viewport = viewportRef.current;
      if (!viewport || typeof ResizeObserver !== 'function') return;
      const observer = new ResizeObserver(() => fit());
      observer.observe(viewport);
      return () => observer.disconnect();
    }, [fit]);

    // Off-screen terminals keep parsing but stop painting.
    useEffect(() => {
      const viewport = viewportRef.current;
      if (!viewport || typeof IntersectionObserver !== 'function') return;
      const observer = new IntersectionObserver(entries => {
        for (const entry of entries) sessionRef.current?.setPaused(!entry.isIntersecting);
      });
      observer.observe(viewport);
      return () => observer.disconnect();
    }, []);

    useEffect(() => {
      if (focused) inputRef.current?.focus({ preventScroll: true });
    }, [focused]);

    const requestControl = useCallback(() => {
      if (!inputAllowed || holder === 'me') return;
      callbacksRef.current.onRequestControl();
    }, [inputAllowed, holder]);

    /** True when the key may be sent; otherwise asks for control (first keypress). */
    const canType = useCallback((): boolean => {
      if (!inputAllowed) {
        setNotice('This terminal is read-only here.');
        return false;
      }
      if (holder !== 'me') {
        requestControl();
        setNotice('Taking control… type again once you have control.');
        return false;
      }
      return true;
    }, [inputAllowed, holder, requestControl]);

    const sendKey = useCallback(
      (key: KeyLike) => {
        if (!canType()) return;
        const latched = ctrlLatched && key.key.length === 1;
        sessionRef.current?.key(latched ? { ...key, ctrlKey: true } : key);
        if (latched) setCtrlLatched(false);
        setScrolledBack(0);
      },
      [canType, ctrlLatched],
    );

    const doPaste = useCallback(
      (text: string) => {
        if (!canType()) return;
        const error = sessionRef.current?.paste(text) ?? null;
        setNotice(error ?? '');
      },
      [canType],
    );

    const onKeyDown = (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (!focused) return;
      const native = event.nativeEvent;
      const action = map(native, 'terminal');
      // Global interceptors (Ctrl+M/O/G/\…, selection copy) bubble to the screens' shortcut handler.
      if (action === 'terminal.paste') return; // the browser fires `paste` with the clipboard text
      if (action !== 'terminal.forward') return;
      if (native.isComposing || event.key === 'Process' || event.key === 'Unidentified' || event.key === 'Dead') return;
      // Shift+PageUp/PageDown scroll history locally, as in most terminals.
      if (event.shiftKey && (event.key === 'PageUp' || event.key === 'PageDown')) {
        event.preventDefault();
        const session = sessionRef.current;
        if (session) setScrolledBack(session.scrollBy((event.key === 'PageUp' ? 1 : -1) * Math.max(1, session.gridSize().rows - 1)));
        return;
      }
      event.preventDefault();
      sendKey(native);
    };

    const onInput = (event: React.FormEvent<HTMLTextAreaElement>) => {
      const target = event.currentTarget;
      if ((event.nativeEvent as InputEvent).isComposing) return;
      const text = target.value;
      target.value = '';
      if (!text || !canType()) return;
      if (ctrlLatched && text.length === 1) return sendKey({ key: text, ctrlKey: true, altKey: false, shiftKey: false, metaKey: false });
      sessionRef.current?.text(text);
    };

    const onCompositionEnd = (event: React.CompositionEvent<HTMLTextAreaElement>) => {
      const text = event.data;
      event.currentTarget.value = '';
      if (text && canType()) sessionRef.current?.text(text);
    };

    const onPaste = (event: React.ClipboardEvent<HTMLTextAreaElement>) => {
      event.preventDefault();
      doPaste(event.clipboardData.getData('text/plain'));
    };

    // Mouse and wheel (non-passive so a consumed wheel does not scroll the page).
    useEffect(() => {
      const viewport = viewportRef.current;
      const screen = screenRef.current;
      const measure = measureRef.current;
      if (!viewport || !screen || !measure) return;
      let buttons: 0 | 1 | 2 | 3 = 3;
      const cellAt = (event: MouseEvent) => {
        const rect = screen.getBoundingClientRect();
        const cell = measure.getBoundingClientRect();
        const width = cell.width / 10 || 1;
        const height = cell.height || 1;
        return { col: Math.floor((event.clientX - rect.left) / width) + 1, row: Math.floor((event.clientY - rect.top) / height) + 1 };
      };
      const report = (event: MouseEvent, kind: 'down' | 'up' | 'move', button: 0 | 1 | 2 | 3) => {
        const session = sessionRef.current;
        if (!session || event.shiftKey) return false; // Shift: local selection, as in xterm
        const { col, row } = cellAt(event);
        return session.mouse({ kind, button, col, row, shift: event.shiftKey, alt: event.altKey, ctrl: event.ctrlKey });
      };
      const toButton = (b: number): 0 | 1 | 2 => (b === 1 ? 1 : b === 2 ? 2 : 0);
      const down = (event: MouseEvent) => {
        buttons = toButton(event.button);
        if (report(event, 'down', buttons)) event.preventDefault();
      };
      const up = (event: MouseEvent) => {
        const button = toButton(event.button);
        buttons = 3;
        if (report(event, 'up', button)) event.preventDefault();
        else if (!window.getSelection()?.toString()) inputRef.current?.focus({ preventScroll: true });
      };
      const move = (event: MouseEvent) => {
        report(event, 'move', event.buttons ? buttons : 3);
      };
      const wheel = (event: WheelEvent) => {
        const session = sessionRef.current;
        if (!session || event.deltaY === 0) return;
        const { col, row } = cellAt(event);
        if (!event.shiftKey && session.mouse({ kind: 'wheel', button: 0, col, row, shift: false, alt: event.altKey, ctrl: event.ctrlKey, wheelDelta: event.deltaY })) {
          event.preventDefault();
          return;
        }
        const before = session.scrolledBack;
        const after = session.scrollBy(event.deltaY < 0 ? WHEEL_LINES : -WHEEL_LINES);
        if (after !== before) event.preventDefault();
        setScrolledBack(after);
      };
      viewport.addEventListener('mousedown', down);
      viewport.addEventListener('mouseup', up);
      viewport.addEventListener('mousemove', move);
      viewport.addEventListener('wheel', wheel, { passive: false });
      return () => {
        viewport.removeEventListener('mousedown', down);
        viewport.removeEventListener('mouseup', up);
        viewport.removeEventListener('mousemove', move);
        viewport.removeEventListener('wheel', wheel);
      };
    }, []);

    const copySelection = useCallback(async () => {
      const screen = screenRef.current;
      const selection = typeof window !== 'undefined' ? window.getSelection() : null;
      if (!screen || !selection || selection.rangeCount === 0) return;
      const range = selection.getRangeAt(0);
      if (!screen.contains(range.commonAncestorContainer)) return;
      const text = selection.toString();
      if (!text) return;
      // Called from a user gesture (button, Ctrl+Y); never from terminal output.
      await navigator.clipboard?.writeText(text);
    }, []);

    const pasteFromClipboard = useCallback(async () => {
      try {
        const text = await navigator.clipboard.readText();
        doPaste(text);
      } catch {
        setNotice('Clipboard access was refused; use your keyboard’s paste instead.');
      }
    }, [doPaste]);

    useImperativeHandle(
      ref,
      () => ({
        fit,
        focus: () => inputRef.current?.focus({ preventScroll: true }),
        copySelection,
        paste: (text: string) => doPaste(text),
        reset: () => {
          sessionRef.current?.scrollToBottom();
          setScrolledBack(0);
          setNotice('');
        },
      }),
      [fit, copySelection, doPaste],
    );

    let overlay: ReactNode = null;
    if (exitCode !== null) overlay = <p className={styles.overlay}>The agent exited{exitCode === 0 ? '' : ` (code ${exitCode})`}.</p>;
    else if (state === 'reconnecting') overlay = <p className={styles.overlay}>Reconnecting… the screen refreshes when the terminal is back.</p>;
    else if (state === 'closed') overlay = <p className={styles.overlay}>Disconnected from this terminal.</p>;
    else if (!ready) overlay = <p className={styles.overlay}>Loading terminal…</p>;

    const rootClass = [styles.terminal, theme === 'light' ? styles.light : styles.dark, reducedMotion ? styles.still : '', holding ? styles.holding : styles.viewing].filter(Boolean).join(' ');

    return (
      <section className={rootClass} aria-label={`Terminal: ${title}`} data-holder={holder}>
        <div className={styles.bar}>
          <span className={styles.status} role="status">
            {statusText(state, holder, inputAllowed)}
          </span>
          {inputAllowed && holder !== 'me' && state === 'open' && (
            <button type="button" onClick={requestControl}>
              Take control
            </button>
          )}
          {scrolledBack > 0 && (
            <button
              type="button"
              onClick={() => {
                sessionRef.current?.scrollToBottom();
                setScrolledBack(0);
              }}
            >
              Back to live
            </button>
          )}
          <button type="button" onClick={() => void copySelection()}>
            Copy
          </button>
        </div>
        <div className={styles.viewport} ref={viewportRef}>
          <div className={styles.screen} ref={screenRef} role="log" aria-label={`${title} output`} aria-live="off" />
          <span className={styles.measure} ref={measureRef} aria-hidden="true">
            WWWWWWWWWW
          </span>
          <textarea
            ref={inputRef}
            className={styles.input}
            aria-label={`Type into ${title}`}
            autoCapitalize="off"
            autoComplete="off"
            autoCorrect="off"
            spellCheck={false}
            readOnly={!inputAllowed}
            onKeyDown={onKeyDown}
            onInput={onInput}
            onCompositionEnd={onCompositionEnd}
            onPaste={onPaste}
          />
          {overlay}
        </div>
        {notice && (
          <p className={styles.notice} role="status">
            {notice}
          </p>
        )}
        {touch && (
          <TouchToolbar
            className={styles.toolbar}
            disabled={!inputAllowed || state !== 'open'}
            ctrlLatched={ctrlLatched}
            onToggleCtrl={() => setCtrlLatched(value => !value)}
            onKey={sendKey}
            onPaste={() => void pasteFromClipboard()}
          />
        )}
      </section>
    );
  });
}

export const TerminalView = createTerminalView(loadGhosttyCore);
