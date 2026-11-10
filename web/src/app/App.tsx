'use client';
import { useCallback, useEffect, useState } from 'react';
import { useConnect } from '@/lib/connect/useConnect';
import { registerShellWorker, type ShellUpdate } from '@/lib/pwa/serviceWorker';
import { ConnectScreen } from '@/screens/connect/ConnectScreen';
import { DaemonClient } from '@/lib/client/daemonClient';
import type { FrontendConnection } from '@/lib/client/frontendSocket';
import type { Grant } from '@/lib/client/grant';
import { WorkspacePicker } from '@/components/WorkspacePicker';
import { Workspace } from '@/components/Workspace';

function Connected({ grant, squadAvailable, reset, revoked }: { grant: Grant; squadAvailable: boolean; reset: () => void; revoked: () => void }) {
  const [client] = useState(() => new DaemonClient(grant));
  const [opened, setOpened] = useState<{ connection: FrontendConnection; context: string | null } | null>(null);
  const onOpen = useCallback((connection: FrontendConnection, context: string | null) => setOpened({ connection, context }), []);
  useEffect(() => client.onRevoked(revoked), [client, revoked]);
  if (!opened) return <WorkspacePicker client={client} onOpen={onOpen} />;
  return <Workspace client={client} connection={opened.connection} launchContext={opened.context} squadAvailable={squadAvailable} reconnect={() => { opened.connection.close('user'); setOpened(null); reset(); }} />;
}
export default function App() {
  const { state, controller } = useConnect();
  const [update, setUpdate] = useState<ShellUpdate | null>(null); const [notice, setNotice] = useState('');
  useEffect(() => registerShellWorker(setUpdate), []);
  const reset = useCallback(() => controller.reset(), [controller]);
  const revoked = useCallback(() => { setNotice('Browser access revoked. Connect browser again to continue.'); controller.reset(); }, [controller]);
  return <>{update && <div className="update-banner" role="status"><span>Update available.</span><button onClick={() => update.apply()}>Reload to update</button></div>}{state.phase !== 'connected' ? <>{notice && <p role="alert">{notice}</p>}<ConnectScreen state={state} controller={controller} /></> : <Connected grant={state.grant} squadAvailable={state.health.listener.features.includes('squad')} reset={reset} revoked={revoked} />}</>;
}
