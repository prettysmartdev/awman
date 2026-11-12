import type { AnsweredBy, CountdownKind, ExecutionPhase } from '@/lib/client/types';
import type { ConnectionState } from '@/lib/client/frontendSocket';

// Presentation wording ported from frontend/program_status_text.rs and the
// TUI workflow dialog. Time and the next engine action stay engine-owned.
export function countdownPresentation(kind: CountdownKind, seconds: number) {
  const fails = kind === 'agent_blocked' || kind === 'agent_error';
  const label = kind === 'agent_done' ? `Agent reported done: advancing in ${seconds}s` : kind === 'agent_blocked' ? `Agent is waiting for input: failing step in ${seconds}s` : kind === 'agent_error' ? `Agent reported an error: failing step in ${seconds}s` : `Auto-advancing in ${seconds}s`;
  return { title: `${seconds % 2 === 0 ? '⚠️' : '🤘'} ${fails ? 'Step fails' : 'Yolo'} in ${seconds}s`, label };
}

/** The tab's execution phase in the words a user would say. */
export function phaseLabel(phase: ExecutionPhase['phase']): string {
  return phase === 'idle' ? 'Ready' : phase === 'running' ? 'Running' : phase === 'done' ? 'Finished' : 'Failed';
}

/** The frontend connection state in the words a user would say. */
export function connectionLabel(state: ConnectionState): string {
  return state === 'open' ? 'Connected' : state === 'connecting' ? 'Connecting' : state === 'reconnecting' ? 'Reconnecting' : 'Disconnected';
}

/**
 * Who closed an engine prompt, for a dismissible notice. Null for this
 * frontend's own answer, which needs no notice. Frontend ids are not shown.
 */
export function answeredByText(by: AnsweredBy, ownFrontendId: string | null | undefined): string | null {
  if (by.by === 'frontend') return by.frontend_id === ownFrontendId ? null : 'Another window answered the prompt.';
  return by.by === 'api' ? 'The prompt was answered through the awman API.' : 'awman closed the prompt.';
}
