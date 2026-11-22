// Live yolo-countdown overlay (design §5.4; parity with the TUI's
// `refresh_yolo_countdown_dialog` in src/frontend/tui/app.rs).
//
// The engine never raises a WorkflowYoloCountdown DialogRequest: it publishes
// countdown state through `yolo` (tab) and `slot_yolo` (per container slot)
// deltas, and each frontend derives the visible, non-answering overlay:
//   - outside a parallel group (no dormant slots) the tab's countdown shows;
//   - during a parallel group only the focused slot, while rendered
//     maximized, shows its own countdown (others show it in their slot bar).
// The overlay is recomputed from state on every render, so it appears and
// disappears with countdown deltas, focus changes and window-state changes.
// Its controls send CancelCountdown / WorkflowRequest, never a DialogResponse.
import type { OpenDialog } from './instanceStore';
import type { ContainerWindowStateWire, SlotSnapshot, TabSnapshot, YoloState } from './types';

export const LIVE_COUNTDOWN_PREFIX = 'live-countdown:';

export function liveCountdownState(
  tab: TabSnapshot,
  focusedSlot: SlotSnapshot | undefined,
  windowOf: (slotId: string) => ContainerWindowStateWire,
): { state: YoloState; slotId: string | null } | null {
  const focusedShown = focusedSlot && windowOf(focusedSlot.slot_id) === 'maximized' ? focusedSlot : undefined;
  const parallel = tab.slots.some(slot => slot.dormant);
  if (!parallel && tab.yolo) return { state: tab.yolo, slotId: null };
  if (focusedShown?.yolo) return { state: focusedShown.yolo, slotId: focusedShown.slot_id };
  return null;
}

/** The overlay as a dialog-shaped value (never sent to the engine). */
export function liveCountdownDialog(
  tab: TabSnapshot,
  focusedSlot: SlotSnapshot | undefined,
  windowOf: (slotId: string) => ContainerWindowStateWire,
): OpenDialog | null {
  const live = liveCountdownState(tab, focusedSlot, windowOf);
  if (!live) return null;
  return {
    dialogId: `${LIVE_COUNTDOWN_PREFIX}${tab.tab_id}:${live.slotId ?? 'tab'}:${live.state.step_name}`,
    tabId: tab.tab_id,
    request: { kind: 'workflow_yolo_countdown', step_name: live.state.step_name, remaining_secs: live.state.remaining_secs, countdown_kind: live.state.kind },
  };
}
