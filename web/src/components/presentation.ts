import type { CountdownKind } from '@/lib/client/types';

// Presentation wording ported from frontend/program_status_text.rs and the
// TUI workflow dialog. Time and the next engine action stay engine-owned.
export function countdownPresentation(kind: CountdownKind, seconds: number) {
  const fails = kind === 'agent_blocked' || kind === 'agent_error';
  const label = kind === 'agent_done' ? `Agent reported done: advancing in ${seconds}s` : kind === 'agent_blocked' ? `Agent is waiting for input: failing step in ${seconds}s` : kind === 'agent_error' ? `Agent reported an error: failing step in ${seconds}s` : `Auto-advancing in ${seconds}s`;
  return { title: `${seconds % 2 === 0 ? '⚠️' : '🤘'} ${fails ? 'Step fails' : 'Yolo'} in ${seconds}s`, label };
}
