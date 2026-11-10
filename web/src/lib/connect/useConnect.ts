'use client';

import { useEffect, useState, useSyncExternalStore } from 'react';
import { ConnectController, type ConnectState } from './controller';

/** One ConnectController for the component's lifetime, started on mount. */
export function useConnect(): { state: ConnectState; controller: ConnectController } {
  const [controller] = useState(() => new ConnectController());
  const state = useSyncExternalStore(controller.state.subscribe.bind(controller.state), controller.state.get.bind(controller.state), () => ({ phase: 'idle' }) as const);
  useEffect(() => {
    void controller.start();
    return () => controller.dispose();
  }, [controller]);
  return { state, controller };
}
