'use client';
import { useEffect, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import { FrontendConnection } from '@/lib/client/frontendSocket';
import type { InstanceSummary, LaunchContextSummary } from '@/lib/client/types';
import { readResume, clearResume } from '@/lib/state/session';
import { LAUNCH_CONTEXT_RECIPE } from '@/lib/connect/recipe';
import { errorText } from './ui';

export function WorkspacePicker({ client, onOpen }: { client: DaemonClient; onOpen: (connection: FrontendConnection, context: string | null) => void }) {
  const [contexts, setContexts] = useState<LaunchContextSummary[]>([]);
  const [instances, setInstances] = useState<InstanceSummary[]>([]);
  const [selected, setSelected] = useState('');
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false);
  useEffect(() => {
    let live = true;
    const resume = readResume();
    if (resume) {
      onOpen(new FrontendConnection(client, resume.instanceId, { role: 'primary', resumeFrontendId: resume.frontendId, termSize: { cols: 100, rows: 30 } }), resume.launchContext);
      return;
    }
    void Promise.all([client.launchContexts(), client.listInstances()]).then(([list, existing]) => {
      if (!live) return;
      setContexts(list.items); setSelected(list.default_id ?? list.items[0]?.id ?? ''); setInstances(existing);
    }).catch(e => { if (live) setError(errorText(e)); });
    return () => { live = false; };
  }, [client, onOpen]);
  async function open(instance?: string) {
    setBusy(true); setError('');
    try {
      const id = instance ?? (await client.createInstance()).id;
      clearResume();
      onOpen(new FrontendConnection(client, id, { role: instance ? 'secondary' : 'primary', ...(instance ? {} : { launchContext: selected }), termSize: { cols: 100, rows: 30 } }), instance ? null : selected);
    } catch (e) { setError(errorText(e)); setBusy(false); }
  }
  return <main className="connect"><section className="connect-card"><h1>Choose a workspace</h1>
    {error && <p role="alert">{error}</p>}
    <label>Workspace<select aria-label="Workspace" value={selected} onChange={e => setSelected(e.target.value)}>{contexts.map(c => <option value={c.id} key={c.id}>{c.label} · {c.root}{c.is_default ? ' (default)' : ''}{c.env_state === 'stale' ? ' · environment stale' : ''}</option>)}</select></label>
    {!contexts.length && <><p>Register a workspace from its owner shell.</p><pre>{LAUNCH_CONTEXT_RECIPE}</pre></>}
    <button disabled={!selected || busy} onClick={() => void open()}>Open workspace</button>
    {instances.length > 0 && <><h2>Existing instances</h2>{instances.map(i => <p key={i.id}>{i.annotations.label ?? i.id} · {i.tab_count} tabs <button disabled={busy} onClick={() => void open(i.id)}>Attach read only</button></p>)}</>}
  </section></main>;
}
