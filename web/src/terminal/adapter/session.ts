// TerminalSession: binds one awman.terminal.v1 connection to one wterm core
// and one DOM renderer. Framework-free so it can be tested with fixtures.
//
// Feeding rules (design §5.5): the core is fed ONLY by the connection's
// snapshot (re-init at the snapshot size + write), output (write) and resize
// frames. Input is never echoed locally: keystrokes go to `connection.input`
// and appear when the agent's PTY output comes back. The session sends input
// only while this viewer holds the input grant.
//
// Untrusted output: after every write the core's side-channel effects are
// drained and dropped — OSC 52 clipboard requests, OSC 7 working directory,
// bells, and terminal query replies (DA/DSR/…: auto-replies would let output
// inject input). Only the OSC 0/2 title is surfaced, sanitised, as text.
import type { TerminalThemeColors } from '@wterm/core';
import type { TerminalConnection, TerminalControl } from '@/lib/client/terminalSocket';
import type { DetectedLink, KeyModes, MouseLike, MouseMode } from '../contract';
import { clampScrollback, themeColors, type CoreFactory, type TerminalCore } from '../core/loadCore';
import { encodeAltScroll, encodeKeyLike, encodeMouse, mouseModeOf, type KeyLike } from '../input/encoder';
import { preparePaste } from '../input/paste';
import { MAX_DETECTED_LINKS, sanitizeTitle } from '../security/sanitize';
import { DomRenderer, readViewport, viewportLinks, type RowModel } from './renderer';

/** Bytes buffered while the WASM core loads; a later snapshot supersedes them. */
export const MAX_PENDING_BYTES = 8 * 1024 * 1024;
const MAX_DRAIN = 256;

export interface SessionCallbacks {
  onTitle?(title: string): void;
  onLinks?(links: DetectedLink[]): void;
  /** Short user-facing status (blocked clipboard write, refused paste, load failure…). */
  onNotice?(message: string): void;
  /** Core loaded / modes or size changed: the view re-reads `modes()`/`size()`. */
  onChange?(): void;
}

export interface SessionOptions {
  coreFactory: CoreFactory;
  scrollbackLines: number;
  theme: 'light' | 'dark';
  /** Paint scheduler (requestAnimationFrame in the browser; sync in tests). */
  schedule?: (paint: () => void) => void;
}

type Pending = { kind: 'snapshot'; cols: number; rows: number; screen: Uint8Array } | { kind: 'output'; bytes: Uint8Array } | { kind: 'resize'; cols: number; rows: number };

export interface SessionModes extends KeyModes {
  mouse: MouseMode;
  sgr: boolean;
  altScreen: boolean;
}

export class TerminalSession {
  private core: (TerminalCore & { dispose?(): void }) | null = null;
  private readonly renderer: DomRenderer;
  private pending: Pending[] = [];
  private pendingBytes = 0;
  private pendingDropped = false;
  private disposed = false;
  private offs: (() => void)[] = [];
  private paintQueued = false;
  private paused = false;
  private viewportOffset = 0;
  private title = '';
  private linksKey = '';
  private rows: RowModel[] = [];
  private theme: TerminalThemeColors;
  private clipboardBlockedNoticed = false;
  private control: TerminalControl;
  private size = { cols: 80, rows: 24 };

  constructor(
    private readonly connection: TerminalConnection,
    container: HTMLElement,
    private readonly options: SessionOptions,
    private readonly callbacks: SessionCallbacks = {},
  ) {
    this.renderer = new DomRenderer(container);
    this.theme = themeColors(options.theme);
    this.control = connection.control.get();
  }

  /** Subscribe to the connection (synchronously, so no frame is missed) and load the core. */
  start(): Promise<void> {
    this.offs.push(
      this.connection.onSnapshot(s => this.feed({ kind: 'snapshot', cols: s.cols, rows: s.rows, screen: s.screen })),
      this.connection.onOutput(bytes => this.feed({ kind: 'output', bytes })),
      this.connection.onResize((cols, rows) => this.feed({ kind: 'resize', cols, rows })),
      this.connection.control.subscribe(control => {
        this.control = control;
      }),
    );
    return this.options
      .coreFactory({ cols: this.size.cols, rows: this.size.rows, scrollbackLines: this.options.scrollbackLines })
      .then(core => {
        if (this.disposed) return core.dispose?.();
        this.core = core;
        core.setThemeColors?.(this.theme);
        const queued = this.pending;
        this.pending = [];
        this.pendingBytes = 0;
        for (const item of queued) this.apply(item);
        if (this.pendingDropped) this.callbacks.onNotice?.('Some output arrived while the terminal was loading and was skipped; the screen corrects itself on the next refresh.');
        this.callbacks.onChange?.();
        this.requestPaint();
      })
      .catch(() => {
        if (!this.disposed) this.callbacks.onNotice?.('The terminal renderer could not be loaded.');
      });
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const off of this.offs) off();
    this.offs = [];
    this.pending = [];
    this.renderer.clear();
    this.core?.dispose?.();
    this.core = null;
  }

  get ready(): boolean {
    return this.core !== null;
  }

  /** Current grid size (from the last snapshot/resize frame). */
  gridSize(): { cols: number; rows: number } {
    return { ...this.size };
  }

  modes(): SessionModes {
    const core = this.core;
    if (!core) return { appCursor: false, bracketedPaste: false, mouse: 'none', sgr: false, altScreen: false };
    return {
      appCursor: core.cursorKeysApp(),
      bracketedPaste: core.bracketedPaste(),
      mouse: mouseModeOf(core.mouseTracking?.() ?? 0),
      sgr: core.mouseSgr?.() ?? false,
      altScreen: core.usingAltScreen(),
    };
  }

  visibleRows(): readonly RowModel[] {
    return this.rows;
  }

  // ── output ────────────────────────────────────────────────────────────────

  private feed(item: Pending): void {
    if (this.disposed) return;
    if (item.kind !== 'output' && !(validDimension(item.cols) && validDimension(item.rows))) return; // malformed size: ignore
    if (this.core) {
      this.apply(item);
      this.requestPaint();
      return;
    }
    if (item.kind === 'snapshot') {
      // A snapshot redraws the whole screen: anything before it is moot.
      this.pending = [item];
      this.pendingBytes = item.screen.length;
      this.pendingDropped = false;
      return;
    }
    const size = item.kind === 'output' ? item.bytes.length : 0;
    if (this.pendingBytes + size > MAX_PENDING_BYTES) {
      this.pendingDropped = true;
      return;
    }
    this.pending.push(item);
    this.pendingBytes += size;
  }

  private apply(item: Pending): void {
    const core = this.core;
    if (!core) return;
    switch (item.kind) {
      case 'snapshot':
        // reset + write: a fresh screen at the hub's size (lagged/reconnect resync).
        this.size = { cols: item.cols, rows: item.rows };
        core.init(item.cols, item.rows);
        core.setScrollbackLines?.(clampScrollback(this.options.scrollbackLines));
        core.setThemeColors?.(this.theme);
        this.viewportOffset = 0;
        core.writeRaw(item.screen);
        this.callbacks.onChange?.();
        break;
      case 'output': {
        const before = this.viewportOffset > 0 ? core.getScrollbackCount() : 0;
        core.writeRaw(item.bytes);
        // Keep a scrolled-back view anchored on the same history lines.
        if (this.viewportOffset > 0) this.viewportOffset = Math.min(core.getScrollbackCount(), this.viewportOffset + Math.max(0, core.getScrollbackCount() - before));
        break;
      }
      case 'resize':
        this.size = { cols: item.cols, rows: item.rows };
        core.resize(item.cols, item.rows);
        this.callbacks.onChange?.();
        break;
    }
    this.drainEffects(core);
  }

  private drainEffects(core: TerminalCore): void {
    // null = unchanged since the last read (the core consumes title updates).
    const raw = core.getTitle();
    const title = raw === null ? this.title : sanitizeTitle(raw);
    if (title !== this.title) {
      this.title = title;
      this.callbacks.onTitle?.(title);
    }
    // OSC 52: never applied. Tell the user once per terminal.
    const clip = core.getClipboardWrite?.();
    if (clip !== null && clip !== undefined && !this.clipboardBlockedNoticed) {
      this.clipboardBlockedNoticed = true;
      this.callbacks.onNotice?.('The agent asked to set your clipboard; awman blocked it. Select text and use Copy instead.');
    }
    core.getWorkingDirectory?.(); // OSC 7: dropped (no navigation, no cwd display).
    core.getBellCount?.();
    core.getShellIntegrationState?.();
    // Query replies are never sent back (output must not cause input).
    for (let i = 0; i < MAX_DRAIN && core.getResponse() !== null; i++);
  }

  // ── painting ──────────────────────────────────────────────────────────────

  setPaused(paused: boolean): void {
    this.paused = paused;
    if (!paused) this.requestPaint();
  }

  setTheme(theme: 'light' | 'dark'): void {
    this.theme = themeColors(theme);
    this.core?.setThemeColors?.(this.theme);
    this.requestPaint();
  }

  setScrollbackLines(lines: number): void {
    this.core?.setScrollbackLines?.(clampScrollback(lines));
    this.requestPaint();
  }

  private requestPaint(): void {
    if (this.paintQueued || this.disposed) return;
    this.paintQueued = true;
    const schedule = this.options.schedule ?? defaultSchedule;
    schedule(() => {
      this.paintQueued = false;
      this.paint();
    });
  }

  /** Paint now (tests and the scheduler). Skipped while paused (off-screen). */
  paint(): void {
    const core = this.core;
    if (!core || this.disposed || this.paused) return;
    const view = readViewport(core, { viewportOffset: this.viewportOffset, theme: this.theme });
    this.viewportOffset = Math.min(this.viewportOffset, view.historyLines);
    this.rows = view.rows;
    this.renderer.paint(view.rows);
    core.clearDirty();
    const links = viewportLinks(view.rows).slice(0, MAX_DETECTED_LINKS);
    const key = JSON.stringify(links);
    if (key !== this.linksKey) {
      this.linksKey = key;
      this.callbacks.onLinks?.(links);
    }
  }

  /** Scroll the view through history (positive = older). Returns the new offset. */
  scrollBy(lines: number): number {
    const core = this.core;
    if (!core || core.usingAltScreen()) return 0;
    this.viewportOffset = Math.max(0, Math.min(core.getScrollbackCount(), this.viewportOffset + Math.trunc(lines)));
    this.requestPaint();
    return this.viewportOffset;
  }

  get scrolledBack(): number {
    return this.viewportOffset;
  }

  scrollToBottom(): void {
    if (this.viewportOffset === 0) return;
    this.viewportOffset = 0;
    this.requestPaint();
  }

  // ── input ─────────────────────────────────────────────────────────────────

  get holding(): boolean {
    return this.control.holder === 'me';
  }

  /** Send bytes to the PTY; false (nothing sent) unless this viewer holds control. */
  private send(bytes: Uint8Array | null): boolean {
    if (!bytes || bytes.length === 0 || !this.holding) return false;
    this.scrollToBottom();
    this.connection.input(bytes);
    return true;
  }

  key(event: KeyLike): boolean {
    const modes = this.modes();
    return this.send(encodeKeyLike(event, modes));
  }

  /** Text typed through an IME, a soft keyboard or the touch toolbar. */
  text(text: string): boolean {
    // eslint-disable-next-line no-control-regex -- typed text never carries escape sequences
    const clean = text.replace(/\r?\n/g, '\r').replace(/[\x00-\x08\x0b-\x0c\x0e-\x1f\x7f-\x9f]/g, '');
    return clean.length > 0 && this.send(new TextEncoder().encode(clean));
  }

  raw(bytes: Uint8Array): boolean {
    return this.send(bytes);
  }

  /** Paste (bracketed when the agent enabled mode 2004). Returns an error message or null. */
  paste(text: string): string | null {
    if (!this.holding) return 'Take control to paste into this terminal.';
    const prepared = preparePaste(text, this.modes().bracketedPaste);
    if (!prepared.ok) return prepared.message;
    this.send(prepared.bytes);
    return null;
  }

  /** Mouse report when the agent enabled tracking; false when the event should act locally (select/scroll). */
  mouse(event: Omit<MouseLike, 'sgr'>): boolean {
    const modes = this.modes();
    if (!this.holding) return false;
    if (modes.mouse !== 'none') {
      const bytes = encodeMouse({ ...event, sgr: modes.sgr }, modes.mouse);
      if (bytes) this.send(bytes);
      return true;
    }
    if (event.kind === 'wheel' && modes.altScreen) {
      // Alternate scroll: wheel → cursor keys on the alternate screen (TUI parity).
      return this.send(encodeAltScroll((event.wheelDelta ?? 0) < 0, modes.appCursor));
    }
    return false;
  }
}

/** The hub clamps PTY sizes (terminal_hub MAX_TERMINAL_COLS/ROWS); never allocate beyond 1000. */
export const MAX_GRID_DIMENSION = 1000;
function validDimension(value: number): boolean {
  return Number.isInteger(value) && value >= 1 && value <= MAX_GRID_DIMENSION;
}

function defaultSchedule(paint: () => void): void {
  if (typeof requestAnimationFrame === 'function') requestAnimationFrame(() => paint());
  else setTimeout(paint, 16);
}
