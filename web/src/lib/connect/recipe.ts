// Owner-side commands the connect screen shows (work item §2, design §5.9).
// The browser cannot run these itself; they are copyable text only.

/** Two-line `awman config set` recipe for the built-in `loopback` listener. */
export const ENABLE_RECIPE = [
  'awman config set daemon.features.engine.enabled true',
  'awman config set daemon.network.listeners.loopback.enabled true',
] as const;

/** Shown as the final step only when the daemon may be stopped. */
export const DAEMON_START = 'awman daemon start --background';

/** Diagnostics: listener addresses, transports, features and bind errors. */
export const DAEMON_STATUS = 'awman daemon status';

/** Launch-context creation, run from the project directory (design §3.4). */
export const LAUNCH_CONTEXT_RECIPE = 'awman daemon api launch_contexts.create default=true';

export const EXPECTED_ENDPOINT = '127.0.0.1:9877';
