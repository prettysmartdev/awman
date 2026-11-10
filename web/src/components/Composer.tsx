'use client';
import { useEffect, useRef, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { FrontendConnection } from '@/lib/client/frontendSocket';
import { completions } from '@/lib/client/catalogue';
import type { Catalogue, TabSnapshot } from '@/lib/client/types';
import type { OutputEntry } from '@/lib/client/instanceStore';
import { useScreenShortcuts as useShortcuts } from './ui';
import { errorText } from './ui';

export function Composer({ client, connection, tab, readOnly, entries, quit, copy }: { client: DaemonClient; connection: FrontendConnection; tab: TabSnapshot; readOnly: boolean; entries: readonly OutputEntry[]; quit: () => void; copy: () => void }) {
  const [text, setText] = useState(''); const [catalogue, setCatalogue] = useState<Catalogue | null>(null); const [error, setError] = useState('');
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
  const parseError = last?.kind === 'finished' ? last.result.error : null;
  function run() {
    if (locked || !text.trim()) return;
    try { submitted.current = text; setPending(true); setError(''); connection.send({ type: 'submit', tab_id: tab.tab_id, input: { source: 'command_box', raw: text }, pty_size: { cols: 100, rows: 30 } }); }
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
    if (action === 'composer.submit') { run(); return; }
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
  return <form className="composer" data-key-context="composer" onSubmit={e => { e.preventDefault(); run(); }}><label htmlFor="command">Command</label><div className="composer-input"><textarea id="command" aria-label="Command" ref={input} value={text} readOnly={locked} placeholder={composerPlaceholder(tab.execution_phase.phase, readOnly)} onChange={e => { setText(e.target.value); base.current = null; index.current = -1; }} /><button disabled={locked || !text.trim()}>Run</button></div>
    {locked && <p className="hint">{readOnly ? 'Read only frontend' : pending ? 'Waiting for the engine to confirm submission…' : 'A command is running. Cancel it before submitting another.'}</p>}
    {(error || parseError) && <p role="alert">{error || `${parseError?.code}: ${parseError?.message}`}</p>}
    <div className="suggestions" aria-label="Command suggestions">{list.slice(0, 12).map(c => <button type="button" className="secondary" key={c.completion} title={c.help} disabled={locked} onClick={() => { setText(text.replace(/\S*$/, '') + c.completion); base.current = null; input.current?.focus(); }}>{c.completion}<span className="hint"> {c.help}</span></button>)}</div>
  </form>;
}

/** TUI parity (render/command_box.rs): "q to quit" ghost text when empty and idle/done. */
export function composerPlaceholder(phase: TabSnapshot['execution_phase']['phase'], readOnly: boolean): string {
  if (readOnly) return 'Read only';
  if (phase === 'running') return 'A command is running';
  if (phase === 'idle' || phase === 'done') return 'q to quit · Type a command; Tab for suggestions';
  return 'Type a command; Tab for suggestions';
}
