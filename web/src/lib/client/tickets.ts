// WebSocket upgrade tickets (design §2.7). Browsers cannot set an
// Authorization header on a WebSocket, so a one-use, 30 s ticket obtained
// with the grant over a no-store fetch rides in Sec-WebSocket-Protocol.
// It is never put in a query string, logged or stored.
import { ProtocolError } from './errors';
import { FRONTEND_PROTOCOL, TERMINAL_PROTOCOL, TICKET_SUBPROTOCOL_PREFIX } from './types';

export type TicketRoute = 'frontend' | 'terminal';

export const TICKET_RE = /^awt_[A-Za-z0-9_-]{16,128}$/;

export function protocolFor(route: TicketRoute): typeof FRONTEND_PROTOCOL | typeof TERMINAL_PROTOCOL {
  return route === 'frontend' ? FRONTEND_PROTOCOL : TERMINAL_PROTOCOL;
}

/** `[real protocol, "awman.ticket.<ticket>"]` — exactly one of each (§2.7 step 2). */
export function ticketSubprotocols(route: TicketRoute, ticket: string): [string, string] {
  if (!TICKET_RE.test(ticket)) throw new ProtocolError('malformed upgrade ticket');
  return [protocolFor(route), `${TICKET_SUBPROTOCOL_PREFIX}${ticket}`];
}

/** The daemon must select only the real protocol, never echo the ticket. */
export function acceptedProtocolOk(route: TicketRoute, selected: string): boolean {
  return selected === protocolFor(route);
}
