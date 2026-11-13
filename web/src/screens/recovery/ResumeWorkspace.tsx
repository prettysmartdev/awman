'use client';
import { useEffect, useState } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import { FrontendConnection, type CloseReason } from '@/lib/client/frontendSocket';
import { RECOVERY_COPY, RECOVERY_TITLES } from '@/lib/connect/model';
import type { ResumeRecord } from '@/lib/state/session';
import { EntryFrame } from '../components/EntryFrame';
import { throttleText, useRecoveryThrottle } from './useRecoveryThrottle';

/** Own the opening connection until its snapshot is ready, then hand it to Workspace. */
export function ResumeWorkspace({ client, resume, onOpen, chooseWorkspace }: {
  client: DaemonClient;
  resume: ResumeRecord;
  onOpen: (connection: FrontendConnection, context: string | null) => void;
  chooseWorkspace: () => void;
}) {
  const [attempt, setAttempt] = useState({ number: 0, role: 'primary' as 'primary' | 'secondary' });
  const [ended, setEnded] = useState<CloseReason | null>(null);
  const throttled = useRecoveryThrottle(client);
  useEffect(() => {
    let handedOff = false;
    const connection = new FrontendConnection(client, resume.instanceId, {
      role: attempt.role,
      ...(attempt.role === 'primary' ? { resumeFrontendId: resume.frontendId, ...(attempt.number ? { manualResume: true } : {}) } : {}),
      termSize: { cols: 100, rows: 30 },
    });
    const ready = () => {
      const view = connection.store.get();
      if (!handedOff && view.snapshot && !view.ended && connection.state.get() === 'open') {
        handedOff = true;
        onOpen(connection, resume.launchContext);
      }
    };
    const offStore = connection.store.subscribe(ready);
    const offState = connection.state.subscribe(ready);
    const offClose = connection.onClose(setEnded);
    if (connection.closeReason) setEnded(connection.closeReason);
    ready();
    return () => {
      offStore(); offState(); offClose();
      if (!handedOff) connection.close('navigation');
    };
  }, [client, resume, onOpen, attempt]);
  function retry(role: 'primary' | 'secondary') {
    setEnded(null);
    setAttempt(previous => ({ number: previous.number + 1, role }));
  }
  const other = ended?.kind === 'other_primary';
  const unavailable = ended?.kind === 'not_found' || ended?.kind === 'grace_expired';
  const auth = ended?.kind === 'auth_ended' ? ended.state : null;
  const title = other ? RECOVERY_TITLES.other_primary : unavailable ? RECOVERY_TITLES.resume_unavailable : auth ? RECOVERY_TITLES[auth] : ended ? 'Could not open this workspace' : 'Reconnecting to your workspace…';
  const explanation = other ? RECOVERY_COPY.other_primary : unavailable ? RECOVERY_COPY.resume_unavailable : auth ? RECOVERY_COPY[auth]
    : ended?.kind === 'refused' ? `${ended.code}: ${ended.error}` : ended?.kind === 'detached' ? ended.reason : ended ? 'The workspace connection closed.' : throttled !== null ? throttleText(throttled) : 'Your browser is authorized. Waiting for the current workspace status. Commands and answers will not be resent.';
  return <EntryFrame title={title} state={ended?.kind ?? 'resuming_workspace'} busy={!ended}>
    <p role={ended ? 'alert' : 'status'}>{explanation}</p>
    <div className="actions">
      {other && <button onClick={() => retry('secondary')}>Attach read only</button>}
      {ended && !unavailable && !auth && <button className={other ? 'secondary' : undefined} onClick={() => retry('primary')}>Resume workspace</button>}
      <button className={unavailable ? undefined : 'secondary'} onClick={chooseWorkspace}>Choose another workspace</button>
    </div>
  </EntryFrame>;
}
