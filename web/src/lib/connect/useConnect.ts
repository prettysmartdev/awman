'use client';

import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { ConnectController, type ConnectState } from './controller';

/** One ConnectController for the component's lifetime, started on mount. */
export function useConnect(): { state: ConnectState; controller: ConnectController } {
  const [controller] = useState(() => new ConnectController());
  const started = useRef(false);
  const mount = useRef(0);
  const state = useSyncExternalStore(controller.state.subscribe.bind(controller.state), controller.state.get.bind(controller.state), () => ({ phase: 'idle' }) as const);
  useEffect(() => {
    const currentMount = ++mount.current;
    if (!started.current) {
      started.current = true;
      void controller.start();
    }
    return () => {
      // Strict Mode runs a setup/cleanup/setup cycle on mount. Keep the
      // controller alive for the second setup; dispose on a real unmount.
      queueMicrotask(() => {
        // The live ref is intentional: the next setup cancels this cleanup.
        // eslint-disable-next-line react-hooks/exhaustive-deps
        if (mount.current === currentMount) controller.dispose();
      });
    };
  }, [controller]);
  return { state, controller };
}
