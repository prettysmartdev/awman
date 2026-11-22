// Public surface of the typed browser client (design §5.4). Screens and the
// terminal lane import from here (and from ./types for wire shapes).
export { DAEMON_HTTP, DAEMON_WS, HttpTransport, supportsTargetAddressSpace, type FetchLike } from './http';
export { REQUIRED_CAPABILITIES, SUPPORTED_PROTOCOLS, classifyHealth, loopbackPermission, missingBrowserApis, probe, type LoopbackPermission, type ProbeResult } from './health';
export { LINK_CODE_RE, createLink, linkCommand, pollLink, type LinkRequest } from './links';
export { clearGrant, currentGrant, isGrantLive, onGrantChange, setGrant, type Grant } from './grant';
export { DaemonClient } from './daemonClient';
export {
  BackpressureError,
  FrontendConnection,
  NotConnectedError,
  WEB_BUILD,
  type CloseReason,
  type ConnectionState,
  type HelloOptions,
  type OutboundFrame,
} from './frontendSocket';
export {
  TerminalConnection,
  type TerminalConnectionState,
  type TerminalControl,
  type TerminalErrorEvent,
  type TerminalSnapshot,
} from './terminalSocket';
export { InstanceStore, type ClientNotice, type InstanceView, type OpenDialog, type OutputEntry } from './instanceStore';
export { applyDelta, applyDeltas } from './reducers';
export { completionFor, completions, type Completion } from './catalogue';
export { DaemonError, DaemonUnreachable, GrantRevoked, LinkDenied, LinkExpired, LinkGone, ProtocolError } from './errors';
export { Subject, type Observable } from './observable';
export { ticketSubprotocols } from './tickets';
export { decodeBase64, encodeBase64 } from './base64';
export type { SocketFactory, WebSocketLike } from './socket';
