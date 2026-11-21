// Pure view model for the connect screen: maps ConnectState to the copy,
// recipe and actions the matrix requires (startup.recipe, .loopback_permission,
// .connect_browser, .version_handshake). Kept separate from React so the
// distinct states are unit-tested exactly.
import { DAEMON_START, DAEMON_STATUS, ENABLE_RECIPE, EXPECTED_ENDPOINT } from './recipe';
import { linkCommand } from '../client/links';
import { RECOVERY_COPY, type RecoveryState } from '../client/recovery';
import type { ConnectNotice, ConnectState, SavedAuthorization } from './controller';

export { RECOVERY_COPY, type RecoveryState } from '../client/recovery';

/**
 * `connect` is “Connect browser” (in `memory_only` it connects for this page
 * only); `cancel_recovery` cancels a running reauthorization;
 * `choose_credential` / `forget_credential` act on `view.choices`
 * (controller.chooseCredential / forgetCredential).
 */
export type ConnectAction = 'allow' | 'recheck' | 'connect' | 'cancel_link' | 'retry_link' | 'cancel_recovery' | 'choose_credential' | 'forget_credential';

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
  | 'connected'
  | 'choose_credential'
  | Exclude<RecoveryState, 'restored' | 'other_primary' | 'resume_unavailable' | 'tab_closed' | 'terminal_view_only'>;

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
  /** The C7 state whose exact explanation is `body[0]`, if any (`restored` when connected by proof). */
  notice: RecoveryState | null;
  /** Saved authorizations to choose from (`choose_credential`): listener and expiry only. */
  choices: SavedAuthorization[];
  /** `rate_limited`: seconds until the automatic retry. */
  retryAfterSec: number | null;
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
  notice: null,
  choices: [],
  retryAfterSec: null,
};

/** Headings for the connect-screen recovery states (the paragraph is the exact C7 text). */
export const RECOVERY_TITLES: Readonly<Record<RecoveryState, string>> = {
  reauthorizing: 'Reconnecting this browser…',
  restored: 'Reconnected',
  authorization_expired: 'Authorization expired',
  authorization_revoked: 'Access revoked',
  daemon_restarted: 'The daemon restarted',
  daemon_stopped: 'The daemon stopped',
  daemon_unreachable: 'awman did not answer',
  other_primary: 'Open in another tab',
  listener_changed: 'The listener changed',
  listener_closed: 'The listener was closed',
  origin_removed: 'This site is no longer allowed',
  origin_mismatch: 'Saved for a different site',
  authorization_unavailable: 'Could not reconnect this browser',
  reauth_unsupported: 'Update awman to stay connected',
  memory_only: 'This browser cannot stay connected',
  resume_unavailable: 'Choose a workspace',
  tab_closed: 'Tab closed',
  terminal_view_only: 'View only',
  rate_limited: 'Waiting to reconnect',
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
      return state.previous
        ? { ...connectView({ phase: 'probed', result: state.previous, notice: state.notice ?? null }), busy: true }
        : { ...BASE, kind: 'starting', title: 'Looking for awman on this machine…', busy: true };
    case 'connected':
      return state.restored
        ? { ...BASE, kind: 'connected', title: RECOVERY_TITLES.restored, body: [RECOVERY_COPY.restored], notice: 'restored' }
        : { ...BASE, kind: 'connected', title: 'Connected' };
    case 'linking':
      return linkingView(state);
    case 'reauthorizing':
      return { ...BASE, kind: 'reauthorizing', title: RECOVERY_TITLES.reauthorizing, body: [RECOVERY_COPY.reauthorizing], notice: 'reauthorizing', actions: ['cancel_recovery'], busy: true };
    case 'choose_credential':
      return state.purpose === 'recover'
        ? {
            ...BASE,
            kind: 'choose_credential',
            title: 'Choose a saved authorization',
            body: ['This browser holds more than one saved authorization for this daemon. Choose the one to reconnect with. No workspace is opened until you choose one.'],
            choices: state.choices,
            actions: ['choose_credential', 'forget_credential', 'connect'],
          }
        : {
            ...BASE,
            kind: 'choose_credential',
            title: 'Forget a saved authorization',
            body: ['This browser already holds the maximum of eight saved authorizations. Forget one before connecting this browser again. Forgetting does not revoke access that is already open in another tab.'],
            choices: state.choices,
            actions: ['forget_credential', 'recheck'],
          };
    case 'probed':
      break;
  }
  return state.notice ? withNotice(probedView(state.result), state.notice) : probedView(state.result);
}

/** Overlay a C7 explanation on the probe view, keeping its setup/repair guidance. */
function withNotice(base: ConnectView, notice: ConnectNotice): ConnectView {
  const state = notice.state;
  const view: ConnectView = {
    ...base,
    kind: state as ConnectViewKind,
    title: RECOVERY_TITLES[state],
    body: [RECOVERY_COPY[state], ...base.body],
    notice: state,
    retryAfterSec: notice.retryAfterSec ?? null,
  };
  switch (state) {
    case 'daemon_stopped':
      return { ...view, startCommand: DAEMON_START, actions: ['recheck'] };
    case 'daemon_unreachable':
      return { ...view, statusHint: base.statusHint ?? STATUS_HINT, actions: base.actions.length ? base.actions : ['recheck'] };
    case 'rate_limited':
      return { ...view, body: [RECOVERY_COPY.rate_limited], actions: ['recheck'] };
    case 'reauth_unsupported':
      return { ...view, statusHint: `Run \`${DAEMON_STATUS}\` to see the daemon version.`, actions: ['recheck', 'connect'] };
    case 'listener_changed':
      return { ...view, actions: base.kind === 'ready_unlinked' ? ['recheck', 'connect'] : base.actions };
    case 'listener_closed':
    case 'origin_removed':
      return { ...view, statusHint: base.statusHint ?? STATUS_HINT };
    default:
      return view;
  }
}

function probedView(result: Extract<ConnectState, { phase: 'probed' }>['result']): ConnectView {
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
