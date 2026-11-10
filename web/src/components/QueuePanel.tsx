'use client';
import { useEffect, useState } from 'react';
import type { FrontendConnection } from '@/lib/client/frontendSocket';
import type { PickRun, QueueEntry, QueueReply, QueueSnapshot } from '@/lib/client/types';
import { Modal, errorText } from './ui';

function replyError(reply: QueueReply): string {
  if (reply.result !== 'error') return 'The engine returned an unexpected queue response.';
  if (reply.value.code === 'taken') return 'This item was claimed by another worker.';
  return reply.value.error ?? 'The work queue could not complete this action.';
}

export function QueuePanel({ connection, tabId, readOnly, onPick, onClose }: {
  connection: FrontendConnection; tabId: string; readOnly: boolean;
  onPick: (run: PickRun) => Promise<void>; onClose: () => void;
}) {
  const [snapshot, setSnapshot] = useState<QueueSnapshot | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [pendingRun, setPendingRun] = useState<PickRun | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  useEffect(() => {
    let live = true;
    async function refresh(force: boolean) {
      try {
        if (force) await connection.queueRequest(tabId, { operation: 'refresh' });
        const reply = await connection.queueRequest(tabId, { operation: 'snapshot' });
        if (!live) return;
        if (reply.result === 'snapshot') setSnapshot(reply.value);
        else setMessage(replyError(reply));
      } catch (error) { if (live) setMessage(errorText(error)); }
    }
    void refresh(true).then(() => connection.queueRequest(tabId, { operation: 'seen' }).catch(() => undefined));
    const timer = setInterval(() => { if (live) void refresh(false); }, 3000);
    return () => { live = false; clearInterval(timer); };
  }, [connection, tabId]);
  const rows = snapshot?.items ?? [];
  const item = rows.find(row => `${row.server}:${row.id}` === selected) ?? null;
  function dismiss() {
    if (busy) return;
    if (pendingRun) void connection.queueRequest(tabId, { operation: 'finish', lease_id: pendingRun.lease_id, choice: 'release' }).catch(() => undefined);
    onClose();
  }
  async function start(run: PickRun) {
    try {
      await onPick(run);
      onClose();
    } finally { setPendingRun(null); }
  }
  async function pick(entry: QueueEntry) {
    if (busy || readOnly) return;
    setBusy(true); setMessage('');
    let claimed: PickRun | null = null;
    try {
      const reply = await connection.queueRequest(tabId, { operation: 'pick', server: entry.server, item_id: entry.id });
      if (reply.result !== 'picked') throw new Error(replyError(reply));
      claimed = reply.value;
      const offer = await connection.queueRequest(tabId, { operation: 'checkout_offer', lease_id: claimed.lease_id });
      if (offer.result !== 'checkout_offer') throw new Error(replyError(offer));
      if (offer.value) setPendingRun(claimed);
      else { const run = claimed; claimed = null; await start(run); }
    } catch (error) {
      if (claimed) void connection.queueRequest(tabId, { operation: 'finish', lease_id: claimed.lease_id, choice: 'release' }).catch(() => undefined);
      setMessage(errorText(error));
    }
    finally { setBusy(false); }
  }
  async function chooseCheckout(choice: 'worktree' | 'fresh_clone') {
    if (!pendingRun || busy) return;
    setBusy(true); setMessage('');
    try {
      const reply = await connection.queueRequest(tabId, { operation: 'checkout', lease_id: pendingRun.lease_id, choice });
      if (reply.result !== 'picked') throw new Error(replyError(reply));
      await start(reply.value);
    } catch (error) { setMessage(errorText(error)); }
    finally { setBusy(false); }
  }
  return <Modal title="Work queue" onDismiss={dismiss}>
    {message && <p role="alert">{message}</p>}
    {snapshot?.error && <p role="alert">{snapshot.error}</p>}
    <p>{snapshot ? `${snapshot.for_you} for you · ${snapshot.new_since_seen} new` : 'Loading work items…'}</p>
    {snapshot?.loaded && !rows.length && <p>No eligible work items.</p>}
    <div className="queue-list" role="list" aria-label="Eligible work items">
      {rows.map(entry => <div role="listitem" key={`${entry.server}:${entry.id}`}>
        <button className="secondary" onClick={() => setSelected(`${entry.server}:${entry.id}`)} aria-pressed={item === entry}>{entry.title} · {entry.server}</button>
        <span>{entry.kind} · priority {entry.priority} · {Math.floor(entry.age_ms / 60000)}m{entry.for_you ? ' · for you' : ''} · {Object.entries(entry.labels).map(([k, v]) => `${k}=${v}`).join(', ') || 'no labels'}</span>
      </div>)}
    </div>
    {pendingRun ? <section><h3>Run in your checkout?</h3><p>{pendingRun.title} belongs to this repository.</p><div className="actions"><button disabled={busy} onClick={() => void chooseCheckout('worktree')}>Run in a worktree of this checkout</button><button disabled={busy} onClick={() => void chooseCheckout('fresh_clone')}>Run in a fresh clone</button></div></section>
      : item && <section><h3>{item.title}</h3>{item.summary && <p>{item.summary}</p>}{item.body && <pre className="prose">{item.body}</pre>}{item.source_url && <p>{item.source_url}</p>}<button disabled={busy || readOnly} onClick={() => void pick(item)}>{busy ? 'Claiming…' : 'Pick'}</button></section>}
    {readOnly && <p className="hint">Attach as primary to pick a work item.</p>}
    <div className="actions"><button className="secondary" disabled={busy} onClick={() => { void connection.queueRequest(tabId, { operation: 'refresh' }).then(() => connection.queueRequest(tabId, { operation: 'snapshot' })).then(reply => { if (reply.result === 'snapshot') setSnapshot(reply.value); else setMessage(replyError(reply)); }).catch(error => setMessage(errorText(error))); }}>Refresh</button></div>
  </Modal>;
}
