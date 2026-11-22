'use client';
import { useEffect, useRef, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { FrontendConnection } from '@/lib/client/frontendSocket';
import { completions } from '@/lib/client/catalogue';
import type { Catalogue, TabSnapshot } from '@/lib/client/types';
import type { OutputEntry } from '@/lib/client/instanceStore';
import { useScreenShortcuts as useShortcuts } from './ui';
import { errorText } from './ui';
import { estimatePtySize } from '@/terminal/geometry';

const coarsePointer = () => typeof matchMedia === 'function' && matchMedia('(pointer: coarse)').matches;

export function Composer({ client, connection, tab, readOnly, entries, quit, copy }: { client: DaemonClient; connection: FrontendConnection; tab: TabSnapshot; readOnly: boolean; entries: readonly OutputEntry[]; quit: () => void; copy: () => void }) {
  const [text, setText] = useState(''); const [catalogue, setCatalogue] = useState<Catalogue | null>(null); const [error, setError] = useState('');
  const [multiline, setMultiline] = useState(false);
  const [pending, setPending] = useState(false); const input = useRef<HTMLTextAreaElement>(null);
  const base = useRef<string | null>(null); const index = useRef(-1); const submitted = useRef<string | null>(null);
  const locked = readOnly || pending || tab.execution_phase.phase === 'running';
  useEffect(() => { let live = true; void client.catalogue().then(c => { if (live) setCatalogue(c); }).catch(e => { if (live) setError(errorText(e)); }); return () => { live = false; }; }, [client]);
  useEffect(() => connection.onFrame(frame => {
    if ((frame.type === 'command_finished' && frame.tab_id === tab.tab_id) || (frame.type === 'delta' && frame.delta === 'phase' && frame.value.tab_id === tab.tab_id)) {
      setPending(false);
      if (frame.type === 'command_finished' && frame.result.error) { setError(`${frame.result.error.code}: ${frame.result.error.message}`); if (submitted.current !== null) setText(submitted.current); }
      else if (submitted.current !== null && (frame.type === 'command_finished' || frame.value.phase.phase === 'running')) { setText(''); if (frame.type === 'command_finished') submitted.current = null; }
    }
  }), [connection, tab.tab_id]);
  useEffect(() => connection.state.subscribe(state => { if (state !== 'open') setPending(false); }), [connection]);
  const list = catalogue ? completions(catalogue, text) : [];
  const last = entries[entries.length - 1];
  const [dismissedParse, setDismissedParse] = useState<OutputEntry | null>(null);
  const parseError = last?.kind === 'finished' && last !== dismissedParse ? last.result.error : null;
  const touch = coarsePointer();
  function run() {
    if (locked || !text.trim()) return;
    try { submitted.current = text; setPending(true); setError(''); connection.send({ type: 'submit', tab_id: tab.tab_id, input: { source: 'command_box', raw: text }, pty_size: estimatePtySize() }); }
    catch (e) { setPending(false); setError(errorText(e)); }
  }
  function complete(direction: number) {
    if (!catalogue || locked) return;
    if (base.current === null) { base.current = text; index.current = direction > 0 ? -1 : 0; }
    const options = completions(catalogue, base.current); if (!options.length) return;
    index.current = (index.current + direction + options.length) % options.length;
    const suggestion = options[index.current]; if (!suggestion) return;
    const prefix = base.current.replace(/\S*$/, ''); setText(prefix + suggestion.completion);
  }
  useShortcuts(() => ({ context: 'composer', opts: { composerEmpty: !text } }), action => {
    if (document.activeElement !== input.current) return false;
    if (action === 'composer.submit') { if (multiline) return false; run(); return; }
    if (action === 'composer.newline') {
      if (locked || !input.current) return;
      const at = input.current.selectionStart; const end = input.current.selectionEnd; setText(text.slice(0, at) + '\n' + text.slice(end));
      requestAnimationFrame(() => { input.current?.setSelectionRange(at + 1, at + 1); }); return;
    }
    if (action === 'composer.completeNext' || action === 'composer.completePrev') { if (locked) return false; complete(action === 'composer.completeNext' ? 1 : -1); return; }
    if (action === 'composer.focusExecution' && text === '') { document.querySelector<HTMLElement>('[aria-label="Command output"]')?.focus(); return; }
    if (action === 'composer.quit') { quit(); return; }
    if (action === 'selection.copy') { copy(); return; }
    return false;
  });
  // Idle: one ~44px row. Suggestions and errors open above it as an overlay,
  // so focusing or erroring never changes the stage (and the agent PTY) height.
  const shownError = error || (parseError ? `${parseError.code}: ${parseError.message}` : '');
  return <form className={`composer ${tab.execution_phase.phase === 'running' ? 'composer-running' : ''}`} data-key-context="composer" onSubmit={e => { e.preventDefault(); run(); }}>
    <div className="composer-popover">
      {shownError && <div className="composer-error" role="alert"><span>{shownError}</span><button type="button" className="secondary" onClick={() => { setError(''); if (last) setDismissedParse(last); }}>Dismiss</button></div>}
      <div className="suggestions" aria-label="Command suggestions">{list.slice(0, 12).map(c => <button type="button" className="secondary" key={c.completion} title={c.help} disabled={locked} onClick={() => { setText(text.replace(/\S*$/, '') + c.completion); base.current = null; input.current?.focus(); }}>{c.completion}<span className="hint"> {c.help}</span></button>)}</div>
    </div>
    <div className="composer-input"><label htmlFor="command" className="sr-only">Command</label><textarea id="command" aria-label="Command" aria-describedby="command-hint" aria-invalid={!!shownError} ref={input} rows={1} value={text} readOnly={locked} placeholder={composerPlaceholder(tab.execution_phase.phase, readOnly, touch)} onChange={e => { setText(e.target.value); base.current = null; index.current = -1; }} /><button type="button" className="secondary composer-multiline" aria-pressed={multiline} onClick={() => setMultiline(v => !v)}>Multi-line</button><button disabled={locked || !text.trim()}>Run</button></div>
    <p className="sr-only" id="command-hint">{!locked ? multiline ? 'Enter inserts a line · Run submits' : touch ? 'Run submits' : 'Enter runs · Shift+Enter inserts a line' : <>{readOnly ? 'Read only frontend' : pending ? 'Waiting for the engine to confirm submission…' : 'A command is running. Stop it before submitting another.'}</>}</p>
  </form>;
}

/** TUI parity (render/command_box.rs): "q to quit" ghost text when empty and idle/done. */
export function composerPlaceholder(phase: TabSnapshot['execution_phase']['phase'], readOnly: boolean, touch = false): string {
  if (readOnly) return 'Read only';
  if (phase === 'running') return 'A command is running';
  // Keyboard-only hints (q, Tab) mean nothing on a touch screen.
  if (touch) return 'Type a command';
  if (phase === 'idle' || phase === 'done') return 'q to quit · Type a command; Tab for suggestions';
  return 'Type a command; Tab for suggestions';
}
