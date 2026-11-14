'use client';
import { useCallback, useEffect, useState } from 'react';
import { useConnect } from '@/lib/connect/useConnect';
import { registerShellWorker, type ShellUpdate } from '@/lib/pwa/serviceWorker';
import { ConnectScreen } from '@/screens/connect/ConnectScreen';
import { DaemonClient } from '@/lib/client/daemonClient';
import type { FrontendConnection } from '@/lib/client/frontendSocket';
import type { Grant } from '@/lib/client/grant';
import type { AuthEndState } from '@/lib/client/recovery';
import { WorkspacePicker } from '@/components/WorkspacePicker';
import { Workspace } from '@/components/Workspace';

function Connected({ grant, squadAvailable, queueAvailable, reset, revoked }: { grant: Grant; squadAvailable: boolean; queueAvailable: boolean; reset: () => void; revoked: (state: AuthEndState) => void }) {
  const [client] = useState(() => new DaemonClient(grant));
  const [opened, setOpened] = useState<{ connection: FrontendConnection; context: string | null } | null>(null);
  const onOpen = useCallback((connection: FrontendConnection, context: string | null) => setOpened({ connection, context }), []);
  useEffect(() => client.onAuthEnded(revoked), [client, revoked]);
  if (!opened) return <WorkspacePicker client={client} onOpen={onOpen} />;
  return <Workspace client={client} connection={opened.connection} launchContext={opened.context} squadAvailable={squadAvailable} queueAvailable={queueAvailable} reconnect={() => { opened.connection.close('user'); setOpened(null); reset(); }} />;
}
export default function App() {
  const { state, controller } = useConnect();
  const [update, setUpdate] = useState<ShellUpdate | null>(null); const [later, setLater] = useState<ShellUpdate | null>(null); const [ended, setEnded] = useState<AuthEndState | null>(null);
  useEffect(() => registerShellWorker(setUpdate), []);
  const reset = useCallback(() => controller.reset(), [controller]);
  // The precise ending is shown by the connect screen while it re-probes; once
  // the connect flow settles it explains the current state itself.
  const revoked = useCallback((state: AuthEndState) => { setEnded(state); controller.reset(); }, [controller]);
  useEffect(() => { if (state.phase !== 'probing' && state.phase !== 'idle') setEnded(null); }, [state.phase]);
  // The banner sits in normal flow above the app bar (the shell shrinks to fit);
  // 'Later' hides it until another update arrives.
  return <>{update && update !== later && <div className="update-banner" role="status"><span>Update available.</span><button onClick={() => update.apply()}>Reload to update</button><button className="secondary" onClick={() => setLater(update)}>Later</button></div>}{state.phase !== 'connected' ? <ConnectScreen state={state} controller={controller} ended={ended} /> : <Connected grant={state.grant} squadAvailable={state.health.listener.features.includes('squad')} queueAvailable={state.health.capabilities.includes('worker_queue')} reset={reset} revoked={revoked} />}</>;
}
