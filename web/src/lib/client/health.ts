// `/v1/health` probe and its classification (design §2.6, §5.4, §5.9).
//
// The probe never concludes that the daemon is *stopped*: an unreachable
// endpoint can equally be a changed port, a bind error, a disabled listener
// or a browser block, so it reports `unreachable` with what was observable.
import { isEpoch, LISTENER_NAME_RE, type ListenerBinding } from './credentialStore';
import { DaemonError, DaemonUnreachable, ProtocolError } from './errors';
import { currentGrant } from './grant';
import { HttpTransport } from './http';
import type { BrowserHealth } from './types';

export const REQUIRED_CAPABILITIES = [
  'engine_instances',
  'frontend_protocol',
  'terminal',
  'named_listeners',
  'browser_links',
  'browser_tickets',
  'launch_contexts',
  'engine_catalogue',
  'web_frontend',
] as const;

/** Protocol versions this build speaks (design §2.6 `protocols`). */
export const SUPPORTED_PROTOCOLS = { api: 1, frontend: 1, terminal: 1, browser: 1 } as const;

export const PROBE_TIMEOUT_MS = 3_000;

/** WI 0160 C2: saved-key recovery protocol this build speaks. Optional: older daemons link memory-only. */
export const REAUTH_PROTOCOL = 1;
export const REAUTH_CAPABILITY = 'browser_reauth';

/**
 * The listener binding a saved key may be enrolled for or proven against, or
 * null when this daemon/listener does not negotiate `browser_reauth:1`
 * (protocol, capability and well-formed epochs are all required).
 */
export function reauthBinding(health: BrowserHealth): ListenerBinding | null {
  const object = health.browser_reauth;
  if (health.protocols.browser_reauth !== REAUTH_PROTOCOL || !health.capabilities.includes(REAUTH_CAPABILITY)) return null;
  if (typeof object !== 'object' || object === null || object.version !== REAUTH_PROTOCOL) return null;
  if (!isEpoch(object.daemon_epoch) || !isEpoch(object.listener_epoch) || !LISTENER_NAME_RE.test(health.listener.name)) return null;
  return { listener: health.listener.name, daemonEpoch: object.daemon_epoch, listenerEpoch: object.listener_epoch };
}

export type ProbeResult =
  | { kind: 'unsupported'; missing: string[] } // this browser lacks an API the app needs
  | { kind: 'unreachable'; detail: 'network' | 'blocked_by_browser' | 'timeout' } // never "daemon stopped"
  | { kind: 'permission_denied' } // Local Network Access denied for this site
  | { kind: 'incompatible'; health: BrowserHealth | null; missing: string[] } // caps/protocols
  | { kind: 'engine_off'; health: BrowserHealth; reason: 'engine_off' | 'engine_draining' | 'listener_without_engine' }
  | { kind: 'ready_unlinked'; health: BrowserHealth } // listener ok, no grant in memory
  | { kind: 'ready'; health: BrowserHealth };

export type LoopbackPermission = 'granted' | 'denied' | 'prompt' | 'unknown';

interface PermissionsLike {
  query(descriptor: { name: string }): Promise<{ state: string }>;
}

/**
 * Local Network Access permission state, when the browser exposes it
 * ('loopback-network' in newer Chromium, 'local-network-access' earlier).
 * 'unknown' when the browser has no such permission (no prompt needed).
 */
export async function loopbackPermission(permissions: PermissionsLike | undefined = globalThis.navigator?.permissions): Promise<LoopbackPermission> {
  if (!permissions) return 'unknown';
  for (const name of ['loopback-network', 'local-network-access', 'local-network']) {
    try {
      const status = await permissions.query({ name });
      if (status.state === 'granted' || status.state === 'denied' || status.state === 'prompt') return status.state;
    } catch {
      // Unknown permission name in this browser: try the next spelling.
    }
  }
  return 'unknown';
}

/** Browser APIs the app cannot work without. */
export function missingBrowserApis(scope: Record<string, unknown> = globalThis as unknown as Record<string, unknown>): string[] {
  const required = ['fetch', 'WebSocket', 'AbortController', 'TextEncoder', 'TextDecoder', 'atob', 'btoa', 'crypto'];
  return required.filter(name => typeof scope[name] === 'undefined');
}

function isBrowserHealth(value: unknown): value is BrowserHealth {
  const body = value as Partial<BrowserHealth> | null;
  return (
    !!body &&
    body.status === 'ok' &&
    typeof body.protocols === 'object' &&
    body.protocols !== null &&
    Array.isArray(body.capabilities) &&
    typeof body.listener === 'object' &&
    body.listener !== null &&
    typeof body.engine === 'object' &&
    body.engine !== null
  );
}

/** Pure classification of a successfully parsed health body (unit-tested). */
export function classifyHealth(health: BrowserHealth, hasLiveGrant: boolean): ProbeResult {
  const missing: string[] = [];
  for (const [name, version] of Object.entries(SUPPORTED_PROTOCOLS)) {
    if (name === 'browser') continue; // checked with browser_links below
    const theirs = health.protocols[name as keyof BrowserHealth['protocols']];
    if (theirs !== version) missing.push(`protocol ${name} v${version}`);
  }
  for (const capability of REQUIRED_CAPABILITIES) {
    if (!health.capabilities.includes(capability)) missing.push(capability);
  }
  const servesEngine = health.listener.features.includes('engine');
  if (servesEngine && health.listener.transport === 'loopback_http' && health.browser_links && health.protocols.browser !== SUPPORTED_PROTOCOLS.browser) {
    missing.push(`protocol browser v${SUPPORTED_PROTOCOLS.browser}`);
  }
  if (missing.length) return { kind: 'incompatible', health, missing };
  if (!servesEngine) return { kind: 'engine_off', health, reason: 'listener_without_engine' };
  if (health.engine.state === 'off') return { kind: 'engine_off', health, reason: 'engine_off' };
  if (health.engine.state === 'draining') return { kind: 'engine_off', health, reason: 'engine_draining' };
  if (!health.browser_links) return { kind: 'incompatible', health, missing: ['browser_links on this listener'] };
  return hasLiveGrant ? { kind: 'ready', health } : { kind: 'ready_unlinked', health };
}

export interface ProbeDeps {
  transport?: HttpTransport;
  permission?: () => Promise<LoopbackPermission>;
  missingApis?: () => string[];
  hasLiveGrant?: () => boolean;
}

/** GET /v1/health on the loaded page's host and classify the outcome. */
export async function probe(signal?: AbortSignal, deps: ProbeDeps = {}): Promise<ProbeResult> {
  const missingApis = (deps.missingApis ?? missingBrowserApis)();
  if (missingApis.length) return { kind: 'unsupported', missing: missingApis };
  const permission = deps.permission ?? (() => loopbackPermission());
  if ((await permission()) === 'denied') return { kind: 'permission_denied' };
  const transport = deps.transport ?? new HttpTransport();
  const hasLiveGrant = deps.hasLiveGrant ?? (() => currentGrant() !== null);
  try {
    const { body } = await transport.json<unknown>('/v1/health', { signal, timeoutMs: PROBE_TIMEOUT_MS });
    if (!isBrowserHealth(body)) return { kind: 'incompatible', health: null, missing: ['browser health body'] };
    return classifyHealth(body, hasLiveGrant());
  } catch (error) {
    if (signal?.aborted) throw error;
    if (error instanceof DaemonUnreachable) {
      // A prompt the user just dismissed/denied surfaces as a network error.
      if ((await permission()) === 'denied') return { kind: 'permission_denied' };
      return { kind: 'unreachable', detail: error.detail };
    }
    if (error instanceof DaemonError || error instanceof ProtocolError) {
      // Something answered, but not with the browser health contract.
      return { kind: 'incompatible', health: null, missing: ['browser health route'] };
    }
    throw error;
  }
}
