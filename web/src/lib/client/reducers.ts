// Pure, immutable application of InstanceDeltas to an InstanceSnapshot
// (design §5.4). Semantics follow the engine's own projection
// (src/command/instance/relay.rs) and the Rust remote presenter
// (src/command/instance/remote.rs::apply_tab_delta):
//   - slot_exited removes the slot; a focused slot that exits moves focus to
//     the last remaining slot (relay.rs close_slot);
//   - group_finished clears `dormant` on the remaining slots;
//   - status_line appends, status_log_reset clears;
//   - slot_acp_update appends to the slot's ACP history (bounded like
//     ACP_HISTORY_LIMIT);
//   - deltas for unknown tabs/slots are ignored (as remote.rs does).
// Unchanged sub-objects keep their identity so React can skip re-rendering.
import type {
  AnnotationTarget,
  Annotations,
  InstanceDelta,
  InstanceSnapshot,
  SlotSnapshot,
  TabSnapshot,
} from './types';

/** Rust: command::instance::feeds::ACP_HISTORY_LIMIT */
export const ACP_HISTORY_LIMIT = 2000;
/** Client-side bound on a tab's status log (render work + memory). */
export const STATUS_LOG_LIMIT = 10_000;

function mapTab(snapshot: InstanceSnapshot, tabId: string, update: (tab: TabSnapshot) => TabSnapshot): InstanceSnapshot {
  const index = snapshot.tabs.findIndex(tab => tab.tab_id === tabId);
  if (index < 0) return snapshot;
  const current = snapshot.tabs[index];
  if (!current) return snapshot;
  const next = update(current);
  if (next === current) return snapshot;
  const tabs = snapshot.tabs.slice();
  tabs[index] = next;
  return { ...snapshot, tabs };
}

function mapSlot(tab: TabSnapshot, slotId: string, update: (slot: SlotSnapshot) => SlotSnapshot): TabSnapshot {
  const index = tab.slots.findIndex(slot => slot.slot_id === slotId);
  if (index < 0) return tab;
  const current = tab.slots[index];
  if (!current) return tab;
  const next = update(current);
  if (next === current) return tab;
  const slots = tab.slots.slice();
  slots[index] = next;
  return { ...tab, slots };
}

function upsert<T>(items: readonly T[], item: T, same: (a: T) => boolean): T[] {
  const index = items.findIndex(same);
  if (index < 0) return [...items, item];
  const next = items.slice();
  next[index] = item;
  return next;
}

function bounded<T>(items: T[], limit: number): T[] {
  return items.length > limit ? items.slice(items.length - limit) : items;
}

function setAnnotations(snapshot: InstanceSnapshot, target: AnnotationTarget, annotations: Annotations): InstanceSnapshot {
  switch (target.target) {
    case 'instance':
      return { ...snapshot, annotations };
    case 'tab':
      return mapTab(snapshot, target.tab_id, tab => ({ ...tab, annotations }));
    case 'frontend': {
      const index = snapshot.frontends.findIndex(f => f.frontend_id === target.frontend_id);
      if (index < 0) return snapshot;
      const frontend = snapshot.frontends[index];
      if (!frontend) return snapshot;
      const frontends = snapshot.frontends.slice();
      frontends[index] = { ...frontend, annotations };
      return { ...snapshot, frontends };
    }
    case 'terminal':
      return snapshot; // terminals are not part of the instance snapshot
  }
}

/** Apply one delta; returns the same object when nothing changed. */
export function applyDelta(snapshot: InstanceSnapshot, delta: InstanceDelta): InstanceSnapshot {
  switch (delta.delta) {
    case 'tab_opened':
      return { ...snapshot, tabs: upsert(snapshot.tabs, delta.value, tab => tab.tab_id === delta.value.tab_id) };
    case 'tab_closed': {
      const tabs = snapshot.tabs.filter(tab => tab.tab_id !== delta.value.tab_id);
      if (tabs.length === snapshot.tabs.length) return snapshot;
      const active_tab_hint = snapshot.active_tab_hint === delta.value.tab_id ? null : snapshot.active_tab_hint;
      return { ...snapshot, tabs, active_tab_hint };
    }
    case 'tab_label':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, label: delta.value.label }));
    case 'phase':
      return mapTab(snapshot, delta.value.tab_id, tab => ({
        ...tab,
        execution_phase: delta.value.phase,
        current_workflow: delta.value.current_workflow,
        current_container: delta.value.current_container,
      }));
    case 'workflow':
      return mapTab(snapshot, delta.value.tab_id, tab => ({
        ...tab,
        workflow: delta.value.view,
        workflow_invocation_id: delta.value.invocation_id,
        context_path: delta.value.context_path,
        worktree_path: delta.value.worktree_path,
      }));
    case 'slot_launched':
      return mapTab(snapshot, delta.value.tab_id, tab => ({
        ...tab,
        slots: upsert(tab.slots, delta.value.slot, slot => slot.slot_id === delta.value.slot.slot_id),
      }));
    case 'slot_container_name':
      return mapTab(snapshot, delta.value.tab_id, tab =>
        mapSlot(tab, delta.value.slot_id, slot => ({ ...slot, container_name: delta.value.container_name })),
      );
    case 'slot_exited':
      return mapTab(snapshot, delta.value.tab_id, tab => {
        const slots = tab.slots.filter(slot => slot.slot_id !== delta.value.slot_id);
        if (slots.length === tab.slots.length) return tab;
        const focused_slot = tab.focused_slot === delta.value.slot_id ? (slots[slots.length - 1]?.slot_id ?? null) : tab.focused_slot;
        return { ...tab, slots, focused_slot };
      });
    case 'slot_stuck':
      return mapTab(snapshot, delta.value.tab_id, tab => mapSlot(tab, delta.value.slot_id, slot => ({ ...slot, stuck: delta.value.stuck })));
    case 'slot_stats':
      return mapTab(snapshot, delta.value.tab_id, tab => mapSlot(tab, delta.value.slot_id, slot => ({ ...slot, stats: delta.value.stats })));
    case 'slot_countdown_kind':
      return mapTab(snapshot, delta.value.tab_id, tab =>
        mapSlot(tab, delta.value.slot_id, slot => ({ ...slot, countdown_kind: delta.value.kind })),
      );
    case 'slot_yolo':
      return mapTab(snapshot, delta.value.tab_id, tab => mapSlot(tab, delta.value.slot_id, slot => ({ ...slot, yolo: delta.value.state })));
    case 'slot_acp_update':
      return mapTab(snapshot, delta.value.tab_id, tab =>
        mapSlot(tab, delta.value.slot_id, slot => {
          const acp = slot.acp ?? { history: [], pending_permission: null };
          return { ...slot, acp: { ...acp, history: bounded([...acp.history, delta.value.update], ACP_HISTORY_LIMIT) } };
        }),
      );
    case 'slot_acp_permission':
      return mapTab(snapshot, delta.value.tab_id, tab =>
        mapSlot(tab, delta.value.slot_id, slot => {
          const acp = slot.acp ?? { history: [], pending_permission: null };
          return { ...slot, acp: { ...acp, pending_permission: delta.value.request } };
        }),
      );
    case 'group_started':
      return snapshot; // presenter-side marker only; slots arrive as slot_launched
    case 'group_finished':
      return mapTab(snapshot, delta.value.tab_id, tab =>
        tab.slots.some(slot => slot.dormant) ? { ...tab, slots: tab.slots.map(slot => (slot.dormant ? { ...slot, dormant: false } : slot)) } : tab,
      );
    case 'focused_slot':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, focused_slot: delta.value.slot_id }));
    case 'yolo':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, yolo: delta.value.state }));
    case 'yolo_mode':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, yolo_mode: delta.value.on }));
    case 'stuck':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, stuck: delta.value.stuck }));
    case 'git_summary':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, git_summary: delta.value.summary }));
    case 'status_line':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, status_log: bounded([...tab.status_log, delta.value.line], STATUS_LOG_LIMIT) }));
    case 'status_log_reset':
      return mapTab(snapshot, delta.value.tab_id, tab => (tab.status_log.length ? { ...tab, status_log: [] } : tab));
    case 'agents_dashboard':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, agents_dashboard: delta.value.data }));
    case 'last_container_summary':
      return mapTab(snapshot, delta.value.tab_id, tab => ({ ...tab, last_container_summary: delta.value.summary }));
    case 'annotations':
      return setAnnotations(snapshot, delta.value.target, delta.value.annotations);
    case 'frontend_attached':
      return { ...snapshot, frontends: upsert(snapshot.frontends, delta.value, f => f.frontend_id === delta.value.frontend_id) };
    case 'frontend_detached': {
      const frontends = snapshot.frontends.filter(f => f.frontend_id !== delta.value.frontend_id);
      return frontends.length === snapshot.frontends.length ? snapshot : { ...snapshot, frontends };
    }
    case 'squad_health':
      return snapshot.squad_health === delta.value ? snapshot : { ...snapshot, squad_health: delta.value };
    case 'active_tab_hint':
      return { ...snapshot, active_tab_hint: delta.value.tab_id };
    case 'dialog_pending':
      return { ...snapshot, pending_dialogs: upsert(snapshot.pending_dialogs, delta.value, d => d.dialog_id === delta.value.dialog_id) };
    case 'dialog_resolved': {
      const pending_dialogs = snapshot.pending_dialogs.filter(d => d.dialog_id !== delta.value.dialog_id);
      return pending_dialogs.length === snapshot.pending_dialogs.length ? snapshot : { ...snapshot, pending_dialogs };
    }
    default: {
      // Exhaustiveness: a new delta kind is a compile error here.
      // At runtime an unknown kind is rejected by validate.ts before it gets
      // here; never return the delta itself as if it were a snapshot.
      const unknown: never = delta;
      void unknown;
      return snapshot;
    }
  }
}

export function applyDeltas(snapshot: InstanceSnapshot, deltas: readonly InstanceDelta[]): InstanceSnapshot {
  return deltas.reduce(applyDelta, snapshot);
}

/**
 * Whether closing this page now could interrupt live engine work: derived
 * from the authoritative, delta-maintained execution state of every tab
 * (including background tabs and workflow containers), not from the
 * snapshot-only `running_jobs` count, which no delta keeps current.
 */
export function hasRunningWork(snapshot: InstanceSnapshot | null): boolean {
  return !!snapshot?.tabs.some(tab => tab.execution_phase.phase === 'running' || tab.slots.length > 0);
}
