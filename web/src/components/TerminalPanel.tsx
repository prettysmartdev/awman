'use client';
import { useEffect, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import { TerminalConnection } from '@/lib/client/terminalSocket';
import { TerminalView, DEFAULT_SCROLLBACK_LINES, type DetectedLink } from '@/terminal/contract';
import { errorText, useObservable } from './ui';

function ConnectedTerminal({ connection, title, focused, onMounted, working = false }: { connection: TerminalConnection; title: string; focused: boolean; onMounted?: () => void; working?: boolean }) {
  const control = useObservable(connection.control);
  const state = useObservable(connection.state);
  const [links, setLinks] = useState<DetectedLink[]>([]);
  const [error, setError] = useState('');
  const [lastOutput, setLastOutput] = useState(() => Date.now());
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => connection.onOutput(() => setLastOutput(Date.now())), [connection]);
  useEffect(() => { const timer = setInterval(() => setNow(Date.now()), 1000); return () => clearInterval(timer); }, []);
  useEffect(() => connection.onError(e => setError(e.message)), [connection]);
  useEffect(() => { onMounted?.(); }, [onMounted]);
  return <div className="terminal-panel" data-key-context="terminal"><div className="hint">{state} · {control.holder === 'me' ? 'You have control' : 'Read only'}</div>
    {working && now - lastOutput >= 30_000 && <p role="status">Working silently · last output {Math.floor((now - lastOutput) / 1000)}s ago</p>}{error && <p role="alert">{error}</p>}
    <TerminalView connection={connection} title={title} focused={focused} control={control} onRequestControl={() => { void connection.takeControl().catch(e => setError(errorText(e))); }} onLinkDetected={setLinks} scrollbackLines={DEFAULT_SCROLLBACK_LINES} touch={true} reducedMotion={typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches} theme="dark" />
    {links.length > 0 && <details><summary>Links detected</summary>{links.filter(link => /^https?:\/\//i.test(link.url)).map((link, i) => <p key={i}><a href={link.url} target="_blank" rel="noopener noreferrer">{link.text}</a></p>)}</details>}
  </div>;
}
export function TerminalPanel({ client, terminalId, title, focused = true, readOnly = false, onMounted, working = false }: { client: DaemonClient; terminalId: string; title: string; focused?: boolean; readOnly?: boolean; onMounted?: () => void; working?: boolean }) {
  const [connection, setConnection] = useState<TerminalConnection | null>(null);
  useEffect(() => {
    const next = new TerminalConnection(client, terminalId, { readOnly });
    setConnection(next);
    return () => next.close();
  }, [client, terminalId, readOnly]);
  return connection ? <ConnectedTerminal connection={connection} title={title} focused={focused} onMounted={onMounted} working={working} /> : <p>Connecting terminal…</p>;
}
