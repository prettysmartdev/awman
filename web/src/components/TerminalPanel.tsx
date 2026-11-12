'use client';
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import { TerminalConnection, type TerminalControl } from '@/lib/client/terminalSocket';
import { TerminalView, DEFAULT_SCROLLBACK_LINES, type TerminalViewHandle, type DetectedLink } from '@/terminal/contract';
import { Menu } from '@/lib/shell/Menu';
import type { ShellAction } from '@/lib/shell/actions';
import { errorText, useObservable } from './ui';

const COARSE = '(pointer: coarse)';
/** The touch key toolbar is for coarse pointers only; desktop keeps the rows. */
function useCoarsePointer(): boolean {
  const [coarse, setCoarse] = useState(() => typeof matchMedia === 'function' && matchMedia(COARSE).matches);
  useEffect(() => {
    if (typeof matchMedia !== 'function') return;
    const query = matchMedia(COARSE);
    const update = () => setCoarse(query.matches);
    update();
    query.addEventListener?.('change', update);
    return () => query.removeEventListener?.('change', update);
  }, []);
  return coarse;
}

interface ChromeProps {
  /** Identity and panel switch, merged into the terminal's single header row. */
  lead?: ReactNode;
  /** Window actions appended to the one overflow menu. */
  windowActions?: readonly ShellAction[];
  /** Host status (e.g. a countdown) laid over the viewport, never in flow. */
  overlay?: ReactNode;
}

function ConnectedTerminal({ connection, title, focused, onMounted, working = false, inputBlocked, lead, windowActions = [], overlay }: ChromeProps & { connection: TerminalConnection; title: string; focused: boolean; onMounted?: () => void; working?: boolean; inputBlocked?: string }) {
  const terminal = useRef<TerminalViewHandle>(null);
  const [showLinks, setShowLinks] = useState(false);
  const liveControl = useObservable(connection.control);
  // Frontend-socket state only gates input; the terminal connection survives it.
  const control = useMemo<TerminalControl>(() => inputBlocked ? { ...liveControl, inputAllowed: false } : liveControl, [liveControl, inputBlocked]);
  const state = useObservable(connection.state);
  const touch = useCoarsePointer();
  const [links, setLinks] = useState<DetectedLink[]>([]);
  const [error, setError] = useState('');
  const [lastOutput, setLastOutput] = useState(() => Date.now());
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => connection.onOutput(() => setLastOutput(Date.now())), [connection]);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 1000); return () => clearInterval(timer); }, []);
  useEffect(() => connection.onError(e => setError(e.message)), [connection]);
  useEffect(() => { onMounted?.(); }, [onMounted]);
  const inputReason = inputBlocked ?? (state !== 'open' ? 'Reconnect to send input.' : !control.inputAllowed ? 'This terminal is read only.' : control.holder !== 'me' ? 'Take control to send input.' : undefined);
  const actions: ShellAction[] = [
    { id: 'interrupt', label: 'Send Ctrl+C', group: 'Agent', matrix: ['term.ctrl_c'], disabled: inputReason, run: () => connection.input(new Uint8Array([3])) },
    { id: 'paste', label: 'Paste', group: 'Agent', matrix: ['term.paste'], disabled: inputReason, run: () => { void (async () => { try { const text = await navigator.clipboard.readText(); terminal.current?.paste(text); } catch { setError('Clipboard access was refused; use your keyboard’s paste instead.'); } })(); } },
    { id: 'copy', label: 'Copy selection', group: 'Agent', matrix: ['term.select_copy'], run: () => { void terminal.current?.copySelection().catch(() => setError('Copy failed.')); } },
    { id: 'live', label: 'Back to live', group: 'Agent', matrix: ['term.scrollback'], run: () => terminal.current?.reset() },
    { id: 'links', label: showLinks ? 'Hide detected links' : `Links detected (${links.length})`, group: 'Agent', matrix: ['term.untrusted_output'], run: () => setShowLinks(v => !v) },
    { id: 'focus', label: 'Focus terminal', group: 'Agent', matrix: ['term.typing'], run: () => terminal.current?.focus() },
    ...windowActions,
  ];
  const silent = working && now - lastOutput >= 30_000;
  const statusLines = (overlay || silent || error || showLinks) ? <>
    {overlay}
    {silent && <p className="terminal-line" role="status">Working silently · last output {Math.floor((now - lastOutput) / 1000)}s ago</p>}
    {error && <div className="terminal-line terminal-error" role="alert"><span>{error}</span><button type="button" className="secondary" onClick={() => setError('')}>Dismiss</button></div>}
    {showLinks && <section className="terminal-links" aria-label="Links detected"><header><h3>Links detected</h3><button type="button" className="secondary" onClick={() => setShowLinks(false)}>Close links</button></header>{!links.length && <p>No links detected.</p>}{links.filter(link => /^https?:\/\//i.test(link.url)).map((link, i) => <p key={i}><a href={link.url} target="_blank" rel="noopener noreferrer">{link.text}</a></p>)}</section>}
  </> : undefined;
  return <div className="terminal-panel" data-key-context="terminal">
    <TerminalView ref={terminal} connection={connection} title={title} focused={focused} control={control} onRequestControl={() => { void connection.takeControl().catch(e => setError(errorText(e))); }} onLinkDetected={setLinks} scrollbackLines={DEFAULT_SCROLLBACK_LINES} touch={touch} reducedMotion={typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches} theme="dark"
      barStart={lead} barEnd={<Menu label="Agent actions" actions={actions} />} overlay={statusLines} />
  </div>;
}
/**
 * One hub terminal. The connection is keyed only on the terminal and on
 * `viewOnly` (a secondary frontend opens it read only); `readOnly` from a
 * reconnecting frontend socket gates input without dropping the connection.
 */
export function TerminalPanel({ client, terminalId, title, focused = true, readOnly = false, viewOnly, onMounted, working = false, lead, windowActions, overlay }: ChromeProps & { client: DaemonClient; terminalId: string; title: string; focused?: boolean; readOnly?: boolean; viewOnly?: boolean; onMounted?: () => void; working?: boolean }) {
  const [connection, setConnection] = useState<TerminalConnection | null>(null);
  const openReadOnly = viewOnly ?? readOnly;
  useEffect(() => {
    const next = new TerminalConnection(client, terminalId, { readOnly: openReadOnly });
    setConnection(next);
    return () => next.close();
  }, [client, terminalId, openReadOnly]);
  const inputBlocked = readOnly && !openReadOnly ? 'Reconnecting — terminal input resumes when the workspace is back.' : undefined;
  return connection ? <ConnectedTerminal connection={connection} title={title} focused={focused} onMounted={onMounted} working={working} inputBlocked={inputBlocked} lead={lead} windowActions={windowActions} overlay={overlay} /> : <p>Connecting terminal…</p>;
}
