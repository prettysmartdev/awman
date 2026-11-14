'use client';
import { useEffect, useRef, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import { FrontendConnection } from '@/lib/client/frontendSocket';
import type { InstanceSummary, LaunchContextSummary } from '@/lib/client/types';
import { readResume, clearResume } from '@/lib/state/session';
import { LAUNCH_CONTEXT_RECIPE } from '@/lib/connect/recipe';
import { EntryFrame } from '@/screens/components/EntryFrame';
import { CommandBlock } from '@/screens/components/CommandBlock';
import { ResumeWorkspace } from '@/screens/recovery/ResumeWorkspace';
import { errorText } from './ui';

export function WorkspacePicker({ client, onOpen }: { client: DaemonClient; onOpen: (connection: FrontendConnection, context: string | null) => void }) {
  const [resume, setResume] = useState(readResume);
  const [contexts, setContexts] = useState<LaunchContextSummary[]>([]);
  const [instances, setInstances] = useState<InstanceSummary[]>([]);
  const [selected, setSelected] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [refresh, setRefresh] = useState(0);
  const opening = useRef(false);
  const live = useRef(true);
  useEffect(() => { live.current = true; return () => { live.current = false; }; }, []);
  useEffect(() => {
    if (resume) return;
    let active = true;
    setLoading(true); setError('');
    void Promise.all([client.launchContexts(), client.listInstances()]).then(([list, existing]) => {
      if (!active) return;
      setContexts(list.items);
      setSelected(previous => list.items.some(c => c.id === previous) ? previous : list.items.find(c => c.id === list.default_id)?.id ?? list.items[0]?.id ?? '');
      setInstances(existing);
    }).catch(e => { if (active) setError(errorText(e)); }).finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [client, resume, refresh]);
  async function open(instance?: string) {
    if (opening.current) return;
    opening.current = true;
    setBusy(true); setError('');
    try {
      const id = instance ?? (await client.createInstance()).id;
      if (!live.current) return;
      clearResume();
      onOpen(new FrontendConnection(client, id, { role: instance ? 'secondary' : 'primary', ...(instance ? {} : { launchContext: selected }), termSize: { cols: 100, rows: 30 } }), instance ? null : selected);
    } catch (e) { if (live.current) { setError(errorText(e)); setBusy(false); } opening.current = false; }
  }
  if (resume) return <ResumeWorkspace client={client} resume={resume} onOpen={onOpen} chooseWorkspace={() => { clearResume(); setResume(null); }} />;
  const context = contexts.find(c => c.id === selected);
  return <EntryFrame title="Choose a workspace" state="workspace_picker" busy={loading || busy}>
    <p>Open a project to start working, or view an existing workspace.</p>
    {error && <p className="entry-error" role="alert">{error}</p>}
    {loading && <p role="status">Loading workspaces…</p>}
    {!!contexts.length && <>
      <label className="workspace-label" htmlFor="entry-workspace">Workspace</label>
      <select id="entry-workspace" value={selected} disabled={busy || loading} aria-describedby="workspace-detail" onChange={e => setSelected(e.target.value)}>{contexts.map(c => <option value={c.id} key={c.id}>{c.label}{c.is_default ? ' (default)' : ''}</option>)}</select>
      {context && <div id="workspace-detail" className="workspace-detail"><code>{context.root}</code><p className="hint">{context.env_state === 'stale' ? 'Environment stale — refresh it from the owner shell if needed.' : 'Environment ready'}{context.is_default ? ' · Default workspace' : ''}</p></div>}
    </>}
    {!loading && !contexts.length && !error && <>
      <p>Register a workspace from its owner shell.</p><p className="hint">Run this from the project directory, then choose Recheck.</p>
      <CommandBlock commands={[LAUNCH_CONTEXT_RECIPE]} label="Workspace registration command" primary />
    </>}
    <div className="actions">
      <button disabled={!context || busy || loading} onClick={() => void open()}>{busy ? 'Opening workspace…' : 'Open workspace'}</button>
      <button className={error && !contexts.length ? undefined : 'secondary'} disabled={busy || loading} onClick={() => setRefresh(value => value + 1)}>Recheck</button>
    </div>
    {!!instances.length && <section className="entry-instances" aria-labelledby="existing-workspaces"><h2 id="existing-workspaces">Existing workspaces</h2><p className="hint">View the current status without taking primary control.</p><ul>{instances.map((instance, index) => <li key={instance.id}>
      <div id={`instance-${index}`}><strong>{instance.annotations.label ?? instance.id}</strong><p className="hint">{instance.tab_count} tabs · {instance.running_jobs} running jobs</p></div>
      <button className="secondary" disabled={busy || loading} aria-describedby={`instance-${index}`} onClick={() => void open(instance.id)}>Attach read only</button>
    </li>)}</ul></section>}
  </EntryFrame>;
}
