// Per-instance view state built from the frontend socket (design §5.4):
// the authoritative InstanceSnapshot (snapshot + ordered deltas), plus the
// bounded presentation feeds that are not part of the snapshot (command
// output per tab, connection notices, recently closed dialogs).
//
// Immutable updates: every change produces a new `InstanceView` object, so
// React can subscribe with useSyncExternalStore(store.subscribe, store.get).
// Nothing here is persisted anywhere.
import { applyDelta } from './reducers';
import type {
  AnsweredBy,
  CommandResultWire,
  DialogId,
  DialogRequest,
  EngineFrame,
  InstanceDelta,
  InstanceSnapshot,
  MessageLevel,
  OutputStream,
  TabId,
} from './types';

/** Bound on retained output LINES per tab (design §5.9: 50k-line log bound). */
export const OUTPUT_LIMIT = 50_000;
/** Bound on retained output characters per tab, whatever the line count. */
export const OUTPUT_CHAR_LIMIT = 8 * 1024 * 1024;
/** A single line longer than this is truncated (marked with …). */
export const MAX_LINE_CHARS = 64 * 1024;
/** Bound on an unterminated escape sequence carried between byte chunks. */
const MAX_ESCAPE_CARRY = 4096;
export const NOTICE_LIMIT = 200;
export const CLOSED_DIALOG_LIMIT = 64;

/**
 * One assembled output line (no newline). CommandBytes are decoded as a
 * stream per tab and stream: split UTF-8 and ANSI sequences are joined across
 * frames, and a line without its newline yet stays `open` and grows in place.
 */
export type OutputEntry =
  | { kind: 'text'; stream: OutputStream; level: MessageLevel; text: string; open?: boolean }
  | { kind: 'finished'; result: CommandResultWire };

interface ByteStreamState {
  decoder: TextDecoder;
  /** Trailing, not yet terminated escape sequence held for the next chunk. */
  carry: string;
}

// ANSI is inert text here: strip complete OSC and CSI sequences, never
// interpret them as markup.
/* eslint-disable no-control-regex -- strip ANSI protocol controls as inert text */
const OSC = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g;
const CSI = /\x1b\[[0-?]*[ -/]*[@-~]/g;
const OTHER_ESCAPE = /\x1b[@-Z\\^_]/g;
const INCOMPLETE_ESCAPE = /\x1b(?:\][^\x07\x1b]*(?:\x1b)?|\[[0-?]*[ -/]*)?$/;
const STRAY_ESC = /\x1b/g;
/* eslint-enable no-control-regex */

/** Strips complete escapes; returns [text, incomplete trailing escape]. */
export function stripAnsi(input: string): [string, string] {
  const tail = INCOMPLETE_ESCAPE.exec(input);
  const carry = tail && tail[0].length <= MAX_ESCAPE_CARRY ? tail[0] : '';
  const body = carry ? input.slice(0, input.length - carry.length) : input;
  return [body.replace(OSC, '').replace(CSI, '').replace(OTHER_ESCAPE, '').replace(STRAY_ESC, ''), carry];
}

function boundLine(text: string): string {
  return text.length > MAX_LINE_CHARS ? `${text.slice(0, MAX_LINE_CHARS - 1)}…` : text;
}

const charsOf = (entry: OutputEntry | undefined) => (entry?.kind === 'text' ? entry.text.length : 0);

/**
 * Appends lines to an immutable tab log, bounded by line count and by total
 * characters. `chars` is the current log's character total (tracked by the
 * store so a bound check never rescans the log); returns the new total.
 */
function appendLines(current: readonly OutputEntry[], chars: number, incoming: OutputEntry[]): [OutputEntry[], number] {
  const merged = current.concat(incoming);
  let total = chars + incoming.reduce((sum, entry) => sum + charsOf(entry), 0);
  let start = 0;
  while (start < merged.length && (merged.length - start > OUTPUT_LIMIT || total > OUTPUT_CHAR_LIMIT)) total -= charsOf(merged[start++]);
  return [start ? merged.slice(start) : merged, total];
}

export interface ClientNotice {
  id: number;
  level: MessageLevel;
  text: string;
  /** 'engine' = Notice frame; 'client' = raised by this client (e.g. unconfirmed submit). */
  source: 'engine' | 'client';
  tabId?: TabId;
}

/** A dialog the engine is asking this frontend to answer. */
export interface OpenDialog {
  dialogId: DialogId;
  tabId: TabId;
  request: DialogRequest;
}

export interface InstanceView {
  /** null until the first Snapshot (and while re-syncing after reconnect). */
  snapshot: InstanceSnapshot | null;
  /** Bumped on every full snapshot (reconnect resync). */
  generation: number;
  outputs: ReadonlyMap<TabId, readonly OutputEntry[]>;
  notices: readonly ClientNotice[];
  dialogs: readonly OpenDialog[];
  /** Recently closed dialogs and who answered (first-answer-wins display). */
  closedDialogs: ReadonlyMap<DialogId, AnsweredBy>;
  /** Engine said the instance is gone (detach / grace expired): show the fatal panel. */
  ended: { reason: string } | null;
}

const EMPTY: InstanceView = {
  snapshot: null,
  generation: 0,
  outputs: new Map(),
  notices: [],
  dialogs: [],
  closedDialogs: new Map(),
  ended: null,
};

function capMap<K, V>(map: Map<K, V>, limit: number): Map<K, V> {
  while (map.size > limit) {
    const first = map.keys().next();
    if (first.done) break;
    map.delete(first.value);
  }
  return map;
}

export class InstanceStore {
  private view: InstanceView = EMPTY;
  private readonly listeners = new Set<() => void>();
  private noticeSeq = 0;
  private readonly byteStreams = new Map<string, ByteStreamState>();
  private readonly outputChars = new Map<TabId, number>();

  private setLog(tabId: TabId, base: readonly OutputEntry[], baseChars: number, incoming: OutputEntry[]): void {
    const [log, chars] = appendLines(base, baseChars, incoming);
    this.outputChars.set(tabId, chars);
    this.commit({ ...this.view, outputs: new Map(this.view.outputs).set(tabId, log) });
  }

  readonly get = (): InstanceView => this.view;

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  private commit(next: InstanceView): void {
    if (next === this.view) return;
    this.view = next;
    for (const listener of [...this.listeners]) listener();
  }

  /** Replace state from a full snapshot (initial or after reconnect). */
  applySnapshot(snapshot: InstanceSnapshot): void {
    const dialogs = snapshot.pending_dialogs.map(d => ({ dialogId: d.dialog_id, tabId: d.tab_id, request: d.request }));
    // Output for tabs that no longer exist is dropped; the rest is kept
    // (the snapshot does not replay output history).
    const live = new Set(snapshot.tabs.map(tab => tab.tab_id));
    const outputs = new Map([...this.view.outputs].filter(([tabId]) => live.has(tabId)));
    for (const key of [...this.byteStreams.keys()]) if (!live.has(key.slice(0, key.lastIndexOf(':')))) this.byteStreams.delete(key);
    for (const tabId of [...this.outputChars.keys()]) if (!live.has(tabId)) this.outputChars.delete(tabId);
    this.commit({ ...this.view, snapshot, generation: this.view.generation + 1, dialogs, outputs, ended: null });
  }

  applyDelta(delta: InstanceDelta): void {
    const current = this.view.snapshot;
    if (!current) return;
    let next: InstanceView = this.view;
    const snapshot = applyDelta(current, delta);
    if (snapshot !== current) next = { ...next, snapshot };
    if (delta.delta === 'dialog_pending') next = this.withDialog(next, { dialogId: delta.value.dialog_id, tabId: delta.value.tab_id, request: delta.value.request });
    if (delta.delta === 'dialog_resolved') next = { ...next, dialogs: next.dialogs.filter(d => d.dialogId !== delta.value.dialog_id) };
    if (delta.delta === 'tab_closed') {
      this.byteStreams.delete(`${delta.value.tab_id}:stdout`);
      this.byteStreams.delete(`${delta.value.tab_id}:stderr`);
      this.outputChars.delete(delta.value.tab_id);
    }
    if (delta.delta === 'tab_closed' && next.outputs.has(delta.value.tab_id)) {
      const outputs = new Map(next.outputs);
      outputs.delete(delta.value.tab_id);
      next = { ...next, outputs };
    }
    this.commit(next);
  }

  private withDialog(view: InstanceView, dialog: OpenDialog): InstanceView {
    if (view.dialogs.some(d => d.dialogId === dialog.dialogId)) return view;
    if (view.closedDialogs.has(dialog.dialogId)) return view;
    return { ...view, dialogs: [...view.dialogs, dialog] };
  }

  /** Output for a tab the snapshot does not (or no longer) contain is dropped. */
  private knownTab(tabId: TabId): boolean {
    return !!this.view.snapshot?.tabs.some(tab => tab.tab_id === tabId);
  }

  /**
   * Appends `text` to a tab's log as lines. An `open` (unterminated) last
   * line of the same stream is continued rather than starting a new row.
   */
  private appendText(tabId: TabId, stream: OutputStream, level: MessageLevel, text: string, keepOpen: boolean): void {
    if (!this.knownTab(tabId)) return;
    const current = this.view.outputs.get(tabId) ?? [];
    const last = current[current.length - 1];
    let base = current;
    let pieces = text.split('\n');
    if (keepOpen && last?.kind === 'text' && last.open && last.stream === stream) {
      base = current.slice(0, -1);
      pieces = [last.text + (pieces[0] ?? ''), ...pieces.slice(1)];
    }
    const open = keepOpen && !text.endsWith('\n');
    if (text.endsWith('\n')) pieces.pop();
    // Only the newest OUTPUT_LIMIT lines can survive; never materialize more.
    const tail = pieces.length > OUTPUT_LIMIT ? pieces.slice(pieces.length - OUTPUT_LIMIT) : pieces;
    const entries: OutputEntry[] = tail.map((line, index) => ({ kind: 'text', stream, level, text: boundLine(line), ...(open && index === tail.length - 1 ? { open: true } : {}) }));
    if (!entries.length && base === current) return;
    const chars = (this.outputChars.get(tabId) ?? 0) - (base === current ? 0 : charsOf(last));
    this.setLog(tabId, base, chars, entries);
  }

  /** Flushes streaming decoders for a tab (command boundary) and closes its open line. */
  private flushBytes(tabId: TabId): void {
    for (const stream of ['stdout', 'stderr'] as const) {
      const state = this.byteStreams.get(`${tabId}:${stream}`);
      if (!state) continue;
      this.byteStreams.delete(`${tabId}:${stream}`);
      const [text] = stripAnsi(state.carry + state.decoder.decode());
      if (text) this.appendText(tabId, stream, 'info', text, true);
    }
    // Command boundary: no line continues into the next command's output.
    const current = this.view.outputs.get(tabId);
    if (current?.some(entry => entry.kind === 'text' && entry.open)) {
      const closed = current.map((entry): OutputEntry => (entry.kind === 'text' && entry.open ? { kind: 'text', stream: entry.stream, level: entry.level, text: entry.text } : entry));
      this.commit({ ...this.view, outputs: new Map(this.view.outputs).set(tabId, closed) });
    }
  }

  /** Apply a non-structural engine frame (output, dialogs, notices, detach). */
  applyFrame(frame: EngineFrame): void {
    switch (frame.type) {
      case 'command_output':
        this.appendText(frame.tab_id, frame.stream, frame.level, frame.text, false);
        return;
      case 'command_finished':
        this.flushBytes(frame.tab_id);
        if (!this.knownTab(frame.tab_id)) return;
        this.setLog(frame.tab_id, this.view.outputs.get(frame.tab_id) ?? [], this.outputChars.get(frame.tab_id) ?? 0, [{ kind: 'finished', result: frame.result }]);
        return;
      case 'dialog_request':
        this.commit(this.withDialog(this.view, { dialogId: frame.dialog_id, tabId: frame.tab_id, request: frame.request }));
        return;
      case 'dialog_closed': {
        const closedDialogs = capMap(new Map(this.view.closedDialogs).set(frame.dialog_id, frame.answered_by), CLOSED_DIALOG_LIMIT);
        this.commit({ ...this.view, closedDialogs, dialogs: this.view.dialogs.filter(d => d.dialogId !== frame.dialog_id) });
        return;
      }
      case 'notice':
        this.notice(frame.level, frame.text, 'engine');
        return;
      case 'detach':
        this.markEnded(frame.reason === 'protocol_error' ? `protocol_error: ${frame.detail}` : frame.reason);
        return;
      default:
        return; // welcome/snapshot/delta are handled by the connection; program_status/ui_ack by subscribers
    }
  }

  /** Raw output bytes (already base64-decoded by the connection). */
  appendBytes(tabId: TabId, stream: OutputStream, bytes: Uint8Array): void {
    if (!this.knownTab(tabId)) return;
    const key = `${tabId}:${stream}`;
    const state = this.byteStreams.get(key) ?? { decoder: new TextDecoder('utf-8'), carry: '' };
    this.byteStreams.set(key, state);
    const [text, carry] = stripAnsi(state.carry + state.decoder.decode(bytes, { stream: true }));
    state.carry = carry;
    if (text) this.appendText(tabId, stream, 'info', text.replace(/\r\n/g, '\n'), true);
  }

  notice(level: MessageLevel, text: string, source: ClientNotice['source'], tabId?: TabId): void {
    const notice: ClientNotice = { id: ++this.noticeSeq, level, text, source, ...(tabId ? { tabId } : {}) };
    const notices = [...this.view.notices, notice];
    this.commit({ ...this.view, notices: notices.length > NOTICE_LIMIT ? notices.slice(notices.length - NOTICE_LIMIT) : notices });
  }

  dismissNotice(id: number): void {
    this.commit({ ...this.view, notices: this.view.notices.filter(n => n.id !== id) });
  }

  markEnded(reason: string): void {
    this.commit({ ...this.view, ended: { reason } });
  }

  /** While re-syncing: keep the last state visible but mark it stale by dropping open dialogs. */
  beginResync(): void {
    this.commit({ ...this.view, dialogs: [] });
  }
}
