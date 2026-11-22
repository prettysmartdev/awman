// Why a workspace connection closed, in the entry flow's own recovery words
// (WI 0160 C7). Shared by the resume screen and the live workspace so both
// explain the same ending the same way.
import type { CloseReason } from '@/lib/client/frontendSocket';
import { RECOVERY_COPY, RECOVERY_TITLES } from '@/lib/connect/model';

export type ClosedRecovery = 'connect' | 'choose_workspace' | 'resume' | 'reconnect';

export interface ClosedConnectionView {
  title: string;
  explanation: string;
  /** The one next step that fits this ending. */
  recovery: ClosedRecovery;
}

/**
 * `fallback` is the store's ended reason, used when the connection gave no
 * classified close reason (for example an engine detach).
 */
export function closedConnectionView(reason: CloseReason | null, fallback?: string): ClosedConnectionView {
  switch (reason?.kind) {
    case 'auth_ended':
      return { title: RECOVERY_TITLES[reason.state], explanation: RECOVERY_COPY[reason.state], recovery: 'connect' };
    case 'other_primary':
      return { title: RECOVERY_TITLES.other_primary, explanation: RECOVERY_COPY.other_primary, recovery: 'resume' };
    case 'not_found':
    case 'grace_expired':
      return { title: RECOVERY_TITLES.resume_unavailable, explanation: RECOVERY_COPY.resume_unavailable, recovery: 'choose_workspace' };
    case 'refused':
      return { title: 'Could not open this workspace', explanation: `${reason.code}: ${reason.error}`, recovery: 'reconnect' };
    case 'detached':
      return { title: 'The engine closed this connection', explanation: reason.reason, recovery: 'reconnect' };
    default:
      return { title: 'The engine closed this connection', explanation: fallback || 'The workspace connection closed.', recovery: 'reconnect' };
  }
}
