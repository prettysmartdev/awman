// Pure view model for the connect screen: maps ConnectState to the copy,
// recipe and actions the matrix requires (startup.recipe, .loopback_permission,
// .connect_browser, .version_handshake). Kept separate from React so the
// distinct states are unit-tested exactly.
import { DAEMON_START, DAEMON_STATUS, ENABLE_RECIPE, EXPECTED_ENDPOINT } from './recipe';
import { linkCommand } from '../client/links';
import type { ConnectState } from './controller';

export type ConnectAction = 'allow' | 'recheck' | 'connect' | 'cancel_link' | 'retry_link';

export type ConnectViewKind =
  | 'starting'
  | 'needs_permission'
  | 'unsupported'
  | 'unreachable'
  | 'blocked_by_browser'
  | 'permission_denied'
  | 'incompatible'
  | 'engine_off'
  | 'ready_unlinked'
  | 'linking'
  | 'link_failed'
  | 'connected';

export interface ConnectView {
  kind: ConnectViewKind;
  title: string;
  /** Plain-text paragraphs (rendered as text, never HTML). */
  body: string[];
  /** Copyable command block, one command per line. */
  recipe: string[] | null;
  /** `awman daemon start --background`, only when the daemon may be stopped. */
  startCommand: string | null;
  /** Diagnostics hint with `awman daemon status`. */
  statusHint: string | null;
  /** Browser link code + copyable approval command. */
  link: { code: string; command: string; expiresAt: string } | null;
  /** Missing capabilities/protocols for an incompatible daemon. */
  missing: string[];
  actions: ConnectAction[];
  busy: boolean;
}

const BASE: Omit<ConnectView, 'kind' | 'title'> = {
  body: [],
  recipe: null,
  startCommand: null,
  statusHint: null,
  link: null,
  missing: [],
  actions: [],
  busy: false,
};

const STATUS_HINT = `Run \`${DAEMON_STATUS}\` to see each listener's address, features and any bind error. If the listener for this host uses a different port or failed to bind, fix that first: this app connects to ${EXPECTED_ENDPOINT}.`;

export function connectView(state: ConnectState): ConnectView {
  switch (state.phase) {
    case 'idle':
      return { ...BASE, kind: 'starting', title: 'Looking for awman on this machine…', busy: true };
    case 'needs_permission':
      return {
        ...BASE,
        kind: 'needs_permission',
        title: 'Allow access to awman on this machine',
        body: [
          `awman runs on your computer. Your browser asks before a website connects to ${EXPECTED_ENDPOINT}.`,
          'Choose “Allow local access”, then allow the browser prompt. Nothing leaves your machine.',
        ],
        actions: ['allow'],
      };
    case 'probing':
      return state.previous ? { ...connectView({ phase: 'probed', result: state.previous }), busy: true } : { ...BASE, kind: 'starting', title: 'Looking for awman on this machine…', busy: true };
    case 'connected':
      return { ...BASE, kind: 'connected', title: 'Connected' };
    case 'linking':
      return linkingView(state);
    case 'probed':
      break;
  }
  const result = state.result;
  switch (result.kind) {
    case 'unsupported':
      return {
        ...BASE,
        kind: 'unsupported',
        title: 'This browser is not supported',
        body: [`It lacks features awman needs (${result.missing.join(', ')}). Use a current version of Chrome, Edge, Firefox or Safari.`],
        actions: ['recheck'],
      };
    case 'permission_denied':
      return {
        ...BASE,
        kind: 'permission_denied',
        title: 'Local access is blocked for this site',
        body: [
          `The browser denied this page access to ${EXPECTED_ENDPOINT}, so it cannot reach awman.`,
          'Open the site settings (the icon next to the address), allow local network access, then choose Recheck.',
        ],
        actions: ['recheck'],
      };
    case 'unreachable':
      if (result.detail === 'blocked_by_browser') {
        return {
          ...BASE,
          kind: 'blocked_by_browser',
          title: 'The browser blocked the connection',
          body: [
            `This browser refused to connect this page to ${EXPECTED_ENDPOINT}. Some browsers or extensions block local connections from websites.`,
            'Try a current Chrome, Edge or Firefox, or allow local access in the site settings, then choose Recheck.',
          ],
          statusHint: STATUS_HINT,
          actions: ['recheck'],
        };
      }
      return {
        ...BASE,
        kind: 'unreachable',
        title: 'awman did not answer',
        body: [
          `Nothing answered on ${EXPECTED_ENDPOINT}${result.detail === 'timeout' ? ' in time' : ''}. The engine or its loopback listener may be off, the daemon may not be running, or the listener may use another port or have failed to bind.`,
          'Run these in a terminal on this machine, then choose Recheck:',
        ],
        recipe: [...ENABLE_RECIPE],
        startCommand: DAEMON_START,
        statusHint: STATUS_HINT,
        actions: ['recheck'],
      };
    case 'incompatible':
      return {
        ...BASE,
        kind: 'incompatible',
        title: 'Update awman to use the web app',
        body: [
          'The awman daemon on this machine does not support this version of the web app. Nothing was sent to it.',
          'Update awman, restart the daemon, then choose Recheck.',
        ],
        missing: result.missing,
        statusHint: `Run \`${DAEMON_STATUS}\` to see the daemon version.`,
        actions: ['recheck'],
      };
    case 'engine_off': {
      const body =
        result.reason === 'engine_draining'
          ? ['The awman engine is shutting down. Wait a moment, then choose Recheck.']
          : result.reason === 'listener_without_engine'
            ? [
                `The listener on ${EXPECTED_ENDPOINT} answered but does not serve the engine. Add \`engine\` to that listener's features (\`awman config set daemon.network.listeners.${result.health.listener.name}.features …\`), or run:`,
              ]
            : ['awman answered, but the engine is off. Run these in a terminal on this machine, then choose Recheck:'];
      return {
        ...BASE,
        kind: 'engine_off',
        title: result.reason === 'engine_draining' ? 'The engine is stopping' : 'Turn on the awman engine',
        body,
        recipe: result.reason === 'engine_draining' ? null : [...ENABLE_RECIPE],
        statusHint: STATUS_HINT,
        actions: ['recheck'],
      };
    }
    case 'ready_unlinked':
    case 'ready':
      return {
        ...BASE,
        kind: 'ready_unlinked',
        title: 'Connect this browser',
        body: [
          'awman is running. To let this browser use it, choose “Connect browser” and approve the request in a terminal on this machine.',
        ],
        actions: ['connect', 'recheck'],
      };
  }
}

function linkingView(state: Extract<ConnectState, { phase: 'linking' }>): ConnectView {
  const link = state.link ? { code: state.link.code, command: linkCommand(state.link), expiresAt: state.link.expiresAt } : null;
  switch (state.status) {
    case 'creating':
      return { ...BASE, kind: 'linking', title: 'Requesting a link code…', busy: true, actions: ['cancel_link'] };
    case 'waiting':
      return {
        ...BASE,
        kind: 'linking',
        title: 'Approve this browser',
        body: ['Run this in a terminal on this machine and answer “y” to approve:'],
        link,
        actions: ['cancel_link'],
      };
    case 'denied':
      return { ...BASE, kind: 'link_failed', title: 'The request was denied', body: ['The link request was denied in the terminal.'], actions: ['retry_link', 'recheck'] };
    case 'expired':
      return { ...BASE, kind: 'link_failed', title: 'The code expired', body: ['The code was not approved in time. Request a new one.'], actions: ['retry_link', 'recheck'] };
    case 'error':
      return {
        ...BASE,
        kind: 'link_failed',
        title: 'Linking failed',
        body: [state.error ?? 'The daemon refused the link request.'],
        statusHint: STATUS_HINT,
        actions: ['retry_link', 'recheck'],
      };
  }
}

/** Shown on the connect screen: first-time installation needs a connection. */
export const OFFLINE_NOTE =
  'After awman has loaded once while online, the installed app opens without an internet connection and still connects to the daemon on this machine. It cannot be installed for the first time while offline.';
