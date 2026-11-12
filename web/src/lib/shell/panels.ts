// Panel interfaces for the workspace shell (foundation → screens lane).
// Each panel is a presentation component fed from the InstanceView (snapshot +
// bounded feeds) and emitting typed frames through FrontendConnection.send.
// The engine stays authoritative: panels hold only view state (focus, scroll,
// sizes). Screens may add props; removing or renaming these is a contract
// change to record in /awman/context/workflow/web-interfaces.md.
import type { ReactNode } from 'react';
import type { FrontendConnection } from '../client/frontendSocket';
import type { DaemonClient } from '../client/daemonClient';
import type { InstanceView, OpenDialog } from '../client/instanceStore';
import type { KeyContext, MapOptions, WebAction } from '../keys/keymap';
import type { TabId } from '../client/types';

/** What every workspace panel receives. */
export interface PanelContext {
  client: DaemonClient;
  connection: FrontendConnection;
  view: InstanceView;
  activeTabId: TabId | null;
  /** True for a secondary frontend: primary-only actions render read-only. */
  readOnly: boolean;
  narrow: boolean;
}

export interface TabBarProps extends PanelContext {
  onSelectTab(tabId: TabId): void;
  onNewTab(): void;
}

export interface ExecutionLogProps extends PanelContext {
  focused: boolean;
  onFocusComposer(): void;
}

export interface ComposerProps extends PanelContext {
  focused: boolean;
  /** Locked while the active tab is running (TUI parity). */
  locked: boolean;
  onFocusExecution(): void;
}

export type StatusRowProps = PanelContext;
export interface GitPanelProps extends PanelContext {
  open: boolean;
  onClose(): void;
}
export interface WorkflowPanelProps extends PanelContext {
  open: boolean;
  onClose(): void;
}
export type ContainerAreaProps = PanelContext;

export interface DialogHostProps extends PanelContext {
  /** Dialogs for the active tab, in engine order (first-answer-wins). */
  dialogs: readonly OpenDialog[];
}

export interface ShortcutSheetProps {
  open: boolean;
  onClose(): void;
}

/** Focus target reported to the keymap (design §5.6 contexts). */
export interface FocusReport {
  context: KeyContext;
  opts: MapOptions;
}

/** Shell-level handler for keymap actions; return false to decline. */
export type ActionHandler = (action: WebAction) => boolean | undefined;

/** Shell presentation only; engine dialogs remain in their original order. */
export interface ShellSlots {
  appBar: ReactNode;
  contextBar: ReactNode;
  stage: ReactNode;
  inspector?: ReactNode;
  composer: ReactNode;
  dialogs?: ReactNode;
  notices?: ReactNode;
  activeAgent?: ReactNode;
  more?: ReactNode;
}
export type InspectorKind = 'workflow' | 'git' | 'queue' | null;
export type NarrowPanel = 'output' | 'agent' | 'workflow' | 'git' | 'more';
