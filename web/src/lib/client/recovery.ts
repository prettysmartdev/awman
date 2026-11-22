// Reload-safe reauthorization: client state keys and their exact English
// explanations (WI 0160 C7). These are client presentation states, not
// engine frames. Framework-free so the connect model, the frontend socket and
// the entry/recovery screens all render the same literal copy.

export type RecoveryState =
  | 'reauthorizing'
  | 'restored'
  | 'authorization_expired'
  | 'authorization_revoked'
  | 'daemon_restarted'
  | 'daemon_stopped'
  | 'daemon_unreachable'
  | 'other_primary'
  | 'listener_changed'
  | 'listener_closed'
  | 'origin_removed'
  | 'origin_mismatch'
  | 'authorization_unavailable'
  | 'reauth_unsupported'
  | 'memory_only'
  | 'resume_unavailable'
  | 'tab_closed'
  | 'terminal_view_only'
  | 'rate_limited';

export const RECOVERY_COPY: Readonly<Record<RecoveryState, string>> = {
  reauthorizing: 'Reconnecting this browser using its saved authorization. No new link approval is needed.',
  restored: 'Reconnected to your workspace. Commands and answers were not resent. Check the current status before repeating an action.',
  authorization_expired: "This browser's 12-hour authorization has expired. Connect this browser again and approve the new link command.",
  authorization_revoked: "This browser's access was revoked. Connect this browser again only if the owner wants to restore access.",
  daemon_restarted: 'The daemon has restarted since this browser was linked. Its previous authorization is no longer valid. Connect this browser again.',
  daemon_stopped: 'The daemon is stopping or has stopped. Start it on this machine, then choose Recheck. You will need to link this browser again after it restarts.',
  daemon_unreachable:
    'The daemon did not answer. It may be stopped, its listener may be off, or the browser may be blocking access. Check awman daemon status, then choose Recheck. Your saved authorization has not been discarded.',
  other_primary: 'Another browser tab or frontend holds primary control of this workspace. You can view it here, return to the primary tab, or open a new workspace.',
  listener_changed: 'The daemon listener has changed since this browser was linked. Connect this browser again on the intended listener.',
  listener_closed: "The daemon listener was disabled or changed, so this browser's authorization ended. Check the listener settings, then choose Recheck.",
  origin_removed: "This site's origin is no longer allowed by the daemon listener. Ask the owner to review the allowed origins, then choose Recheck.",
  origin_mismatch: 'This authorization belongs to a different site origin. Return to the original site or connect this browser here with a new approval.',
  authorization_unavailable: 'This browser could not recover its saved authorization. Connect this browser again and approve the new link command.',
  reauth_unsupported: 'This daemon does not support saved browser authorization for this app version. Update awman, then choose Recheck.',
  memory_only: 'This browser cannot save its authorization key. You can connect for this page, but reloading will require another link approval.',
  resume_unavailable: 'Your browser is authorized, but the previous workspace connection can no longer be resumed. Choose a workspace to continue. No command has been restarted.',
  tab_closed: "The previously selected tab has closed. Showing the workspace's current tab.",
  terminal_view_only: 'This terminal is view only after reconnecting. Request control to type or resize it.',
  rate_limited: 'Reconnection is temporarily rate limited. Waiting before trying again.',
};

/** Ways an authorization can end for this document (subset of RecoveryState). */
export type AuthEndState = Extract<
  RecoveryState,
  | 'authorization_expired'
  | 'authorization_revoked'
  | 'daemon_restarted'
  | 'daemon_stopped'
  | 'listener_changed'
  | 'listener_closed'
  | 'origin_removed'
  | 'origin_mismatch'
  | 'authorization_unavailable'
  | 'daemon_unreachable'
>;

/**
 * WebSocket 1008 close reasons the daemon uses for a grant ending
 * (`BrowserRevocation::close_reason`). Anything else is not evidence of a
 * specific ending and must be classified with a proof instead.
 */
export function endStateForCloseReason(reason: string | undefined): AuthEndState | null {
  switch (reason) {
    case 'grant revoked':
      return 'authorization_revoked';
    case 'grant expired':
      return 'authorization_expired';
    case 'origin removed':
      return 'origin_removed';
    case 'listener closed':
      return 'listener_closed';
    case 'daemon stopping':
      return 'daemon_stopped';
    default:
      return null;
  }
}

/** Endings after which the saved key for that authorization can never work again. */
export function endDiscardsCredential(state: AuthEndState): boolean {
  return state !== 'daemon_stopped' && state !== 'authorization_unavailable' && state !== 'daemon_unreachable';
}
