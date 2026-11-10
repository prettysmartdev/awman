// The ONE place web shortcuts are defined (design §5.6). Mirrors
// src/frontend/tui/keymap.rs (`map_key`) plus the dialog/event-loop keys the
// TUI handles outside `map_key`, with browser-reserved alternates:
//   Ctrl+T → Alt+T, Ctrl+W → Alt+W, Ctrl+N → Alt+N, Ctrl+Shift+C → button only,
//   Ctrl+A/Ctrl+D → Alt+←/Alt+→ while a text input has focus.
// Every binding names the action-matrix rows it satisfies
// (web/acceptance/action-matrix.json); test/lib/keymap.test.ts checks that
// every matrix row with a `browser.shortcut` resolves through `map()`.
//
// Shortcuts only *select* an action. What the action does (e.g. whether
// Ctrl+C cancels a command, closes a tab or asks to quit) is decided by the
// engine through the frontend protocol, exactly as for the TUI.

export type KeyContext = 'global' | 'composer' | 'execution' | 'dialog' | 'terminal' | 'squadList';

/** Which dialog is focused; dialog-specific keys apply only to their scope. */
export type DialogScope =
  | 'yes_no'
  | 'yes_no_cancel'
  | 'text'
  | 'multiline'
  | 'list'
  | 'new_tab'
  | 'mount_scope'
  | 'config'
  | 'control_board'
  | 'step_confirm'
  | 'yolo'
  | 'quit_confirm'
  | 'close_tab_confirm'
  | 'key_setup_notice'
  | 'other';

export type WebAction =
  // global
  | 'tab.new' | 'tab.prev' | 'tab.next' | 'container.cycle' | 'container.focusNextSlot' | 'containers.detach'
  | 'workflow.overview' | 'workflow.overviewLeft' | 'workflow.overviewRight' | 'workflow.control' | 'workflow.copyContextPath'
  | 'git.toggle' | 'config.open' | 'queue.open' | 'interrupt' | 'selection.copy' | 'shortcuts.open'
  // composer
  | 'composer.submit' | 'composer.newline' | 'composer.completeNext' | 'composer.completePrev' | 'composer.focusExecution' | 'composer.quit'
  // execution log
  | 'exec.focusComposer' | 'exec.stopWatch' | 'exec.scrollUp' | 'exec.scrollDown' | 'exec.pageUp' | 'exec.pageDown'
  | 'exec.top' | 'exec.bottom' | 'exec.toggleStatusLog'
  // generic navigation (lists, config, squad grid)
  | 'nav.up' | 'nav.down' | 'nav.left' | 'nav.right' | 'nav.pageUp' | 'nav.pageDown'
  // dialogs
  | 'dialog.submit' | 'dialog.dismiss' | 'dialog.yes' | 'dialog.no' | 'dialog.cancel'
  | 'dialog.quit' | 'dialog.closeTab' | 'newTab.squad' | 'mountScope.gitRoot' | 'mountScope.cwd'
  | 'config.newEntry' | 'keySetup.copyCommand' | 'keySetup.copyAlternate'
  // workflow control board
  | 'wfc.nextNewContainer' | 'wfc.nextSameContainer' | 'wfc.restart' | 'wfc.back' | 'wfc.finish'
  | 'wfc.retryFailedPeer' | 'wfc.pause' | 'wfc.abort'
  // squad list
  | 'squad.detail' | 'squad.history' | 'squad.attach' | 'squad.new' | 'squad.edit' | 'squad.pause'
  | 'squad.resume' | 'squad.trigger' | 'squad.cancel' | 'squad.delete'
  // terminal
  | 'terminal.forward' | 'terminal.paste';

/** The subset of KeyboardEvent the map needs (plain objects work in tests). */
export interface KeyEventLike {
  key: string;
  code?: string;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  metaKey: boolean;
  repeat?: boolean;
  isComposing?: boolean;
}

export interface MapOptions {
  /** Focused dialog's key scope (context 'dialog' only). */
  dialog?: DialogScope;
  /** Focus is in an editable text field (composer, dialog input, config editor). */
  textInput?: boolean;
  /** Composer text is empty (enables `q` quit and `?` shortcut list). */
  composerEmpty?: boolean;
  /** The workflow overview is visible and wider than the screen. */
  overviewHScroll?: boolean;
  /** Number of visible container slots on the active tab. */
  containers?: number;
  /** The active tab is running `agents status --watch`. */
  watching?: boolean;
}

interface Chord {
  key: string; // normalised: lowercase letters, 'ArrowUp', 'Enter', 'Escape', ',', '\\', …
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
}

export interface Binding {
  action: WebAction;
  /** Human label for the shortcut sheet. */
  label: string;
  /** Primary chords as the TUI spells them, e.g. 'Ctrl+T', '↑', 'g'. */
  keys: readonly string[];
  /** Browser alternates that always work (e.g. 'Alt+T'). */
  alternates?: readonly string[];
  /** Primary chords the browser/OS usually keeps for itself. */
  reserved?: readonly string[];
  /** Contexts the binding applies in; 'dialog' bindings may narrow by scope. */
  contexts: readonly KeyContext[];
  dialogScopes?: readonly DialogScope[];
  /** Extra condition on MapOptions. */
  when?: (opts: MapOptions) => boolean;
  /** Matrix rows (web/acceptance/action-matrix.json) this binding satisfies. */
  matrix: readonly string[];
  /** Group heading on the shortcut sheet. */
  group: 'Global' | 'Command box' | 'Execution log' | 'Dialogs' | 'Workflow' | 'Squad' | 'Terminal';
  /** No key at all in the browser: shown as a button only. */
  buttonOnly?: boolean;
}

const NON_DIALOG: readonly KeyContext[] = ['global', 'composer', 'execution', 'terminal', 'squadList'];
const ALL: readonly KeyContext[] = [...NON_DIALOG, 'dialog'];
const BODY: readonly KeyContext[] = ['composer', 'execution', 'terminal'];
const notInText = (o: MapOptions) => !o.textInput;

// Order matters inside a precedence tier: first match wins.
// Tier 1 = dialog-scope bindings, tier 2 = context bindings, tier 3 = global.
const DIALOG_BINDINGS: readonly Binding[] = [
  { action: 'newTab.squad', label: 'Open the squad tab', keys: ['Ctrl+S'], contexts: ['dialog'], dialogScopes: ['new_tab'], matrix: ['repo.new_tab_squad'], group: 'Dialogs' },
  { action: 'mountScope.gitRoot', label: 'Mount the Git root', keys: ['g'], contexts: ['dialog'], dialogScopes: ['mount_scope'], matrix: ['repo.mount_scope', 'dialog.mount_scope'], group: 'Dialogs' },
  { action: 'mountScope.cwd', label: 'Mount the current directory', keys: ['c'], contexts: ['dialog'], dialogScopes: ['mount_scope'], matrix: ['repo.mount_scope', 'dialog.mount_scope'], group: 'Dialogs' },
  { action: 'dialog.yes', label: 'Yes', keys: ['y'], contexts: ['dialog'], dialogScopes: ['yes_no', 'yes_no_cancel'], matrix: ['dialog.yes_no', 'dialog.yes_no_cancel', 'dialog.workflow_cancel_confirm'], group: 'Dialogs' },
  { action: 'dialog.no', label: 'No', keys: ['n'], contexts: ['dialog'], dialogScopes: ['yes_no', 'yes_no_cancel'], matrix: ['dialog.yes_no', 'dialog.yes_no_cancel', 'dialog.workflow_cancel_confirm'], group: 'Dialogs' },
  { action: 'dialog.cancel', label: 'Cancel', keys: ['c'], contexts: ['dialog'], dialogScopes: ['yes_no_cancel'], matrix: ['dialog.yes_no_cancel'], group: 'Dialogs' },
  { action: 'dialog.submit', label: 'Submit (multi-line)', keys: ['Ctrl+Enter', 'Shift+Enter'], alternates: ['Ctrl+S'], contexts: ['dialog'], dialogScopes: ['multiline'], matrix: ['dialog.multiline_input'], group: 'Dialogs' },
  { action: 'dialog.quit', label: 'Quit awman', keys: ['Ctrl+C'], contexts: ['dialog'], dialogScopes: ['quit_confirm', 'close_tab_confirm'], matrix: ['quit.quit', 'dialog.quit_confirm', 'dialog.close_tab_confirm'], group: 'Dialogs' },
  { action: 'dialog.closeTab', label: 'Close this tab', keys: ['Ctrl+T'], alternates: ['Alt+T'], reserved: ['Ctrl+T'], contexts: ['dialog'], dialogScopes: ['close_tab_confirm'], matrix: ['tabs.close', 'dialog.close_tab_confirm'], group: 'Dialogs' },
  { action: 'keySetup.copyCommand', label: 'Copy the setup command', keys: ['c'], contexts: ['dialog'], dialogScopes: ['key_setup_notice'], matrix: ['dialog.key_setup_notice'], group: 'Dialogs' },
  { action: 'keySetup.copyAlternate', label: 'Copy the alternate command', keys: ['z'], contexts: ['dialog'], dialogScopes: ['key_setup_notice'], matrix: ['dialog.key_setup_notice'], group: 'Dialogs' },
  { action: 'config.newEntry', label: 'New map entry', keys: ['Ctrl+N'], alternates: ['Alt+N'], reserved: ['Ctrl+N'], contexts: ['dialog'], dialogScopes: ['config'], matrix: ['config.new_entry'], group: 'Dialogs' },
  // Workflow control board (wfc.*)
  { action: 'wfc.nextNewContainer', label: 'Next step, new container', keys: ['→'], contexts: ['dialog'], dialogScopes: ['control_board'], matrix: ['wfc.next_new_container'], group: 'Workflow' },
  { action: 'wfc.nextSameContainer', label: 'Next step, same container', keys: ['↓'], contexts: ['dialog'], dialogScopes: ['control_board'], matrix: ['wfc.next_same_container'], group: 'Workflow' },
  { action: 'wfc.restart', label: 'Restart the step', keys: ['↑'], contexts: ['dialog'], dialogScopes: ['control_board'], matrix: ['wfc.restart'], group: 'Workflow' },
  { action: 'wfc.back', label: 'Back one step', keys: ['←'], contexts: ['dialog'], dialogScopes: ['control_board'], matrix: ['wfc.back'], group: 'Workflow' },
  { action: 'wfc.finish', label: 'Finish the workflow', keys: ['Enter'], contexts: ['dialog'], dialogScopes: ['control_board'], matrix: ['wfc.finish'], group: 'Workflow' },
  { action: 'wfc.retryFailedPeer', label: 'Retry failed peer', keys: ['r'], contexts: ['dialog'], dialogScopes: ['control_board'], matrix: ['wfc.retry_failed_peer'], group: 'Workflow' },
  { action: 'wfc.pause', label: 'Pause the workflow', keys: ['p'], contexts: ['dialog'], dialogScopes: ['control_board'], matrix: ['wfc.pause'], group: 'Workflow' },
  { action: 'wfc.abort', label: 'Abort the workflow', keys: ['Ctrl+C'], contexts: ['dialog'], dialogScopes: ['control_board'], matrix: ['wfc.abort'], group: 'Workflow' },
  // Yolo countdown keeps tab switching and slot cycling (TUI event loop).
  { action: 'tab.prev', label: 'Previous tab', keys: ['Ctrl+A'], alternates: ['Alt+←'], contexts: ['dialog'], dialogScopes: ['yolo'], matrix: ['appr.yolo_countdown_tab_switch'], group: 'Workflow' },
  { action: 'tab.next', label: 'Next tab', keys: ['Ctrl+D'], alternates: ['Alt+→'], contexts: ['dialog'], dialogScopes: ['yolo'], matrix: ['appr.yolo_countdown_tab_switch'], group: 'Workflow' },
  { action: 'container.focusNextSlot', label: 'Focus the next container', keys: ['Ctrl+S'], contexts: ['dialog'], dialogScopes: ['yolo'], matrix: ['appr.yolo_countdown_slot_cycle'], group: 'Workflow' },
  { action: 'interrupt', label: 'Cancel the workflow', keys: ['Ctrl+C'], contexts: ['dialog'], dialogScopes: ['yolo', 'step_confirm'], matrix: ['appr.cancel_workflow_confirm'], group: 'Workflow' },
  // Generic dialog navigation and Enter/Esc.
  { action: 'nav.up', label: 'Move up', keys: ['↑'], contexts: ['dialog'], dialogScopes: ['list', 'config', 'other'], matrix: ['dialog.list_picker', 'dialog.kind_select', 'config.navigate'], group: 'Dialogs' },
  { action: 'nav.down', label: 'Move down', keys: ['↓'], contexts: ['dialog'], dialogScopes: ['list', 'config', 'other'], matrix: ['dialog.list_picker', 'dialog.kind_select', 'config.navigate'], group: 'Dialogs' },
  { action: 'nav.pageUp', label: 'Page up', keys: ['PgUp'], contexts: ['dialog'], dialogScopes: ['list', 'config', 'other'], matrix: ['config.navigate'], group: 'Dialogs' },
  { action: 'nav.pageDown', label: 'Page down', keys: ['PgDn'], contexts: ['dialog'], dialogScopes: ['list', 'config', 'other'], matrix: ['config.navigate'], group: 'Dialogs' },
  { action: 'nav.left', label: 'Move left', keys: ['←'], contexts: ['dialog'], dialogScopes: ['config'], when: notInText, matrix: ['config.navigate'], group: 'Dialogs' },
  { action: 'nav.right', label: 'Move right', keys: ['→'], contexts: ['dialog'], dialogScopes: ['config'], when: notInText, matrix: ['config.navigate'], group: 'Dialogs' },
  {
    action: 'dialog.submit', label: 'Confirm / submit', keys: ['Enter'], contexts: ['dialog'],
    dialogScopes: ['text', 'list', 'new_tab', 'config', 'step_confirm', 'key_setup_notice', 'other'],
    matrix: ['repo.new_tab_submit', 'dialog.text_input', 'dialog.secret_input', 'dialog.list_picker', 'dialog.kind_select', 'config.edit_begin', 'config.edit_save', 'appr.step_confirm_advance', 'dialog.workflow_step_confirm', 'dialog.key_setup_notice'],
    group: 'Dialogs',
  },
  {
    action: 'dialog.dismiss', label: 'Dismiss / cancel', keys: ['Esc'], contexts: ['dialog'],
    matrix: ['repo.new_tab_cancel', 'repo.new_tab_failure', 'repo.mount_scope', 'dialog.mount_scope', 'dialog.yes_no', 'dialog.yes_no_cancel', 'dialog.text_input', 'dialog.secret_input', 'dialog.multiline_input', 'dialog.list_picker', 'dialog.kind_select', 'dialog.workflow_cancel_confirm', 'dialog.quit_confirm', 'dialog.close_tab_confirm', 'dialog.workflow_step_confirm', 'dialog.workflow_yolo_countdown', 'dialog.key_setup_notice', 'config.edit_cancel', 'config.close', 'wfc.dismiss', 'appr.step_confirm_pause', 'appr.yolo_countdown_cancel', 'dialog.custom', 'dialog.review'],
    group: 'Dialogs',
  },
];

const CONTEXT_BINDINGS: readonly Binding[] = [
  // Workflow overview horizontal scroll (only while a too-wide overview shows).
  { action: 'workflow.overviewLeft', label: 'Scroll workflow overview left', keys: ['Shift+←'], contexts: BODY, when: o => !!o.overviewHScroll, matrix: ['wf.overview_hscroll'], group: 'Workflow' },
  { action: 'workflow.overviewRight', label: 'Scroll workflow overview right', keys: ['Shift+→'], contexts: BODY, when: o => !!o.overviewHScroll, matrix: ['wf.overview_hscroll'], group: 'Workflow' },
  // Command box
  { action: 'exec.stopWatch', label: 'Stop watching agents', keys: ['Esc'], contexts: ['composer', 'execution'], when: o => !!o.watching, matrix: ['startup.initial_tab_agents_watch', 'exec.agents_watch_stop'], group: 'Execution log' },
  { action: 'composer.newline', label: 'New line', keys: ['Shift+Enter'], alternates: ['Ctrl+Enter'], contexts: ['composer'], matrix: ['cmd.newline'], group: 'Command box' },
  { action: 'composer.submit', label: 'Run the command', keys: ['Enter'], contexts: ['composer'], matrix: ['cmd.submit'], group: 'Command box' },
  { action: 'composer.completePrev', label: 'Previous completion', keys: ['Shift+Tab'], contexts: ['composer'], matrix: ['cmd.autocomplete_prev'], group: 'Command box' },
  { action: 'composer.completeNext', label: 'Next completion', keys: ['Tab'], contexts: ['composer'], matrix: ['cmd.autocomplete_next'], group: 'Command box' },
  { action: 'composer.focusExecution', label: 'Focus the execution log', keys: ['↑'], contexts: ['composer'], matrix: ['cmd.focus_execution'], group: 'Command box' },
  { action: 'composer.quit', label: 'Quit (empty command box)', keys: ['q'], contexts: ['composer'], when: o => !!o.composerEmpty, matrix: ['cmd.q_quit'], group: 'Command box' },
  { action: 'shortcuts.open', label: 'Show keyboard shortcuts', keys: ['?'], contexts: ['composer'], when: o => !!o.composerEmpty, matrix: ['keys.shortcut_list'], group: 'Global' },
  // Execution log
  { action: 'exec.focusComposer', label: 'Back to the command box', keys: ['Esc'], contexts: ['execution'], matrix: ['exec.focus_back'], group: 'Execution log' },
  { action: 'exec.scrollUp', label: 'Scroll up', keys: ['↑'], contexts: ['execution'], matrix: ['exec.scroll'], group: 'Execution log' },
  { action: 'exec.scrollDown', label: 'Scroll down', keys: ['↓'], contexts: ['execution'], matrix: ['exec.scroll'], group: 'Execution log' },
  { action: 'exec.pageUp', label: 'Page up', keys: ['PgUp'], contexts: ['execution'], matrix: ['exec.scroll'], group: 'Execution log' },
  { action: 'exec.pageDown', label: 'Page down', keys: ['PgDn'], contexts: ['execution'], matrix: ['exec.scroll'], group: 'Execution log' },
  { action: 'exec.top', label: 'Jump to the beginning', keys: ['b'], contexts: ['execution'], matrix: ['exec.scroll'], group: 'Execution log' },
  { action: 'exec.bottom', label: 'Jump to the end', keys: ['e'], contexts: ['execution'], matrix: ['exec.scroll'], group: 'Execution log' },
  { action: 'exec.toggleStatusLog', label: 'Toggle the status log', keys: ['l'], contexts: ['execution'], matrix: ['exec.status_log_toggle'], group: 'Execution log' },
  // Squad list (keymap.rs map_squad_list_key)
  { action: 'nav.up', label: 'Move up', keys: ['↑'], contexts: ['squadList'], matrix: ['squad.grid_nav'], group: 'Squad' },
  { action: 'nav.down', label: 'Move down', keys: ['↓'], contexts: ['squadList'], matrix: ['squad.grid_nav'], group: 'Squad' },
  { action: 'nav.left', label: 'Move left', keys: ['←'], contexts: ['squadList'], matrix: ['squad.grid_nav'], group: 'Squad' },
  { action: 'nav.right', label: 'Move right', keys: ['→'], contexts: ['squadList'], matrix: ['squad.grid_nav'], group: 'Squad' },
  { action: 'nav.pageUp', label: 'Page up', keys: ['PgUp'], contexts: ['squadList'], matrix: [], group: 'Squad' },
  { action: 'nav.pageDown', label: 'Page down', keys: ['PgDn'], contexts: ['squadList'], matrix: [], group: 'Squad' },
  { action: 'squad.detail', label: 'Task details', keys: ['Enter'], contexts: ['squadList'], matrix: ['squad.detail'], group: 'Squad' },
  { action: 'squad.history', label: 'Run history', keys: ['h'], contexts: ['squadList'], matrix: ['squad.history'], group: 'Squad' },
  { action: 'squad.attach', label: 'Attach to the running task', keys: ['a'], contexts: ['squadList'], matrix: ['squad.attach'], group: 'Squad' },
  { action: 'squad.new', label: 'New task', keys: ['n'], contexts: ['squadList'], matrix: ['squad.new'], group: 'Squad' },
  { action: 'squad.edit', label: 'Edit task', keys: ['e'], contexts: ['squadList'], matrix: ['squad.edit'], group: 'Squad' },
  { action: 'squad.pause', label: 'Pause task', keys: ['p'], contexts: ['squadList'], matrix: ['squad.pause'], group: 'Squad' },
  { action: 'squad.resume', label: 'Resume task', keys: ['r'], contexts: ['squadList'], matrix: ['squad.resume'], group: 'Squad' },
  { action: 'squad.trigger', label: 'Trigger a run now', keys: ['t'], contexts: ['squadList'], matrix: ['squad.trigger'], group: 'Squad' },
  { action: 'squad.cancel', label: 'Cancel the running task', keys: ['c'], contexts: ['squadList'], matrix: ['squad.cancel'], group: 'Squad' },
  { action: 'squad.delete', label: 'Delete task', keys: ['d'], contexts: ['squadList'], matrix: ['squad.delete'], group: 'Squad' },
  // Terminal: paste is handled by the terminal view; everything not
  // intercepted globally is forwarded to the agent (see map()).
  { action: 'terminal.paste', label: 'Paste into the agent', keys: ['Ctrl+V', 'Shift+Insert'], contexts: ['terminal'], matrix: ['term.paste'], group: 'Terminal' },
  { action: 'terminal.forward', label: 'Interrupt the agent (sent to the terminal)', keys: ['Ctrl+C'], contexts: ['terminal'], matrix: ['term.ctrl_c'], group: 'Terminal' },
];

const GLOBAL_BINDINGS: readonly Binding[] = [
  { action: 'tab.new', label: 'New tab', keys: ['Ctrl+T'], alternates: ['Alt+T'], reserved: ['Ctrl+T'], contexts: ALL, matrix: ['repo.new_tab_dialog'], group: 'Global' },
  { action: 'tab.prev', label: 'Previous tab', keys: ['Ctrl+A'], alternates: ['Alt+←'], contexts: NON_DIALOG, matrix: ['tabs.prev'], group: 'Global' },
  { action: 'tab.next', label: 'Next tab', keys: ['Ctrl+D'], alternates: ['Alt+→'], contexts: NON_DIALOG, matrix: ['tabs.next'], group: 'Global' },
  { action: 'container.cycle', label: 'Cycle the container window', keys: ['Ctrl+M'], contexts: NON_DIALOG, matrix: ['ctr.window_cycle'], group: 'Global' },
  { action: 'workflow.overview', label: 'Toggle the workflow overview', keys: ['Ctrl+O'], contexts: NON_DIALOG, matrix: ['wf.overview_toggle'], group: 'Global' },
  {
    action: 'workflow.control', label: 'Workflow control board', keys: ['Ctrl+W'], alternates: ['Alt+W'], reserved: ['Ctrl+W'], contexts: ALL,
    matrix: ['wfc.open', 'appr.step_confirm_board', 'appr.yolo_countdown_board', 'dialog.workflow_yolo_countdown', 'dialog.workflow_step_confirm'], group: 'Global',
  },
  { action: 'git.toggle', label: 'Toggle the Git sidebar', keys: ['Ctrl+G'], contexts: ALL, matrix: ['git.toggle'], group: 'Global' },
  { action: 'containers.detach', label: 'Detach from containers (they keep running)', keys: ['Ctrl+\\'], contexts: ALL, matrix: ['ctr.detach', 'squad.detach'], group: 'Global' },
  { action: 'config.open', label: 'Show configuration', keys: ['Ctrl+,'], contexts: ALL, matrix: ['config.open'], group: 'Global' },
  { action: 'queue.open', label: 'Command queue', keys: ['Ctrl+Q'], contexts: NON_DIALOG, matrix: ['queue.panel'], group: 'Global' },
  {
    action: 'container.focusNextSlot', label: 'Focus the next container', keys: ['Ctrl+S'], contexts: BODY,
    when: o => (o.containers ?? 0) > 1, matrix: ['ctr.focus_next_slot'], group: 'Global',
  },
  {
    action: 'selection.copy', label: 'Copy the selection', keys: ['Ctrl+Y'], contexts: ['composer', 'execution', 'terminal', 'squadList'],
    matrix: ['cmd.copy_selection', 'exec.select_copy', 'term.select_copy'], group: 'Global',
  },
  {
    // Ctrl+C reaches the agent while a terminal has focus (handled in map()).
    action: 'interrupt', label: 'Cancel / close tab / quit', keys: ['Ctrl+C'], contexts: ['global', 'composer', 'execution', 'squadList', 'dialog'],
    matrix: ['tabs.close_confirm', 'tabs.close_last_quits', 'quit.confirm', 'quit.cancel_command', 'appr.cancel_workflow_confirm', 'exec.agents_watch_stop', 'dialog.quit_confirm'],
    group: 'Global',
  },
  {
    action: 'workflow.copyContextPath', label: 'Copy the workflow context path', keys: [], reserved: ['Ctrl+Shift+C'], contexts: [], buttonOnly: true,
    matrix: [], group: 'Workflow',
  },
];

export const BINDINGS: readonly Binding[] = [...DIALOG_BINDINGS, ...CONTEXT_BINDINGS, ...GLOBAL_BINDINGS];

/** Keys the terminal never forwards because the app intercepts them (TUI parity). */
export const TERMINAL_INTERCEPTS: readonly WebAction[] = [
  'tab.new', 'tab.prev', 'tab.next', 'container.cycle', 'workflow.overview', 'workflow.control', 'git.toggle',
  'containers.detach', 'config.open', 'container.focusNextSlot', 'selection.copy', 'workflow.overviewLeft', 'workflow.overviewRight',
];

const NAMED_KEYS: Record<string, string> = {
  esc: 'Escape', escape: 'Escape', enter: 'Enter', tab: 'Tab', space: ' ', insert: 'Insert', delete: 'Delete', backspace: 'Backspace',
  home: 'Home', end: 'End', pgup: 'PageUp', pgdn: 'PageDown', pageup: 'PageUp', pagedown: 'PageDown',
  up: 'ArrowUp', down: 'ArrowDown', left: 'ArrowLeft', right: 'ArrowRight',
  '↑': 'ArrowUp', '↓': 'ArrowDown', '←': 'ArrowLeft', '→': 'ArrowRight',
};

/** Parse 'Ctrl+Shift+T', 'Alt+←', 'Esc', 'g', 'Ctrl+\\'. Throws on unknown names. */
export function parseChord(text: string): Chord {
  const parts = text === '+' ? ['+'] : text.split('+');
  const chord: Chord = { key: '', ctrl: false, alt: false, shift: false };
  parts.forEach((raw, index) => {
    const part = raw.trim();
    const last = index === parts.length - 1;
    const lower = part.toLowerCase();
    if (!last) {
      if (lower === 'ctrl') chord.ctrl = true;
      else if (lower === 'alt') chord.alt = true;
      else if (lower === 'shift') chord.shift = true;
      else throw new Error(`unknown modifier in ${text}`);
      return;
    }
    if (NAMED_KEYS[lower] !== undefined) chord.key = NAMED_KEYS[lower];
    else if (part.length === 1) chord.key = part.toLowerCase();
    else throw new Error(`unknown key in ${text}`);
  });
  return chord;
}

/** Normalise a browser key event to a chord (layout-independent for Ctrl/Alt letters). */
export function chordOf(event: KeyEventLike): Chord {
  let key = event.key;
  const code = event.code ?? '';
  if (event.ctrlKey || event.altKey) {
    // macOS Alt+T yields '†'; use the physical key for letters/digits/punctuation.
    const letter = /^Key([A-Z])$/.exec(code);
    const digit = /^Digit([0-9])$/.exec(code);
    if (letter?.[1]) key = letter[1].toLowerCase();
    else if (digit?.[1]) key = digit[1];
    else if (code === 'Backslash') key = '\\';
    else if (code === 'Comma') key = ',';
  }
  if (key.length === 1) key = key.toLowerCase();
  if (key === 'Esc') key = 'Escape';
  return { key, ctrl: event.ctrlKey, alt: event.altKey, shift: event.shiftKey };
}

function isPrintableSymbol(key: string): boolean {
  return key.length === 1 && !/[a-z0-9]/.test(key);
}

function matches(binding: Chord, actual: Chord): boolean {
  if (binding.key !== actual.key || binding.ctrl !== actual.ctrl || binding.alt !== actual.alt) return false;
  // '?', ',' etc. may need Shift on some layouts; don't require an exact Shift for unmodified symbols.
  if (!binding.ctrl && !binding.alt && isPrintableSymbol(binding.key)) return true;
  return binding.shift === actual.shift;
}

const chordCache = new Map<string, Chord>();
function cachedChord(text: string): Chord {
  let chord = chordCache.get(text);
  if (!chord) {
    chord = parseChord(text);
    chordCache.set(text, chord);
  }
  return chord;
}

function bindingMatches(binding: Binding, chord: Chord): boolean {
  return [...binding.keys, ...(binding.alternates ?? [])].some(text => matches(cachedChord(text), chord));
}

function applies(binding: Binding, context: KeyContext, opts: MapOptions): boolean {
  if (!binding.contexts.includes(context)) return false;
  if (binding.dialogScopes && (context !== 'dialog' || !opts.dialog || !binding.dialogScopes.includes(opts.dialog))) return false;
  return binding.when ? binding.when(opts) : true;
}

/**
 * Map a key event in a focus context to an action, or null when the key is
 * not a shortcut (let the browser/text field handle it). Port of
 * keymap.rs::map_key with dialog scopes and browser alternates.
 */
export function map(event: KeyEventLike, context: KeyContext, opts: MapOptions = {}): WebAction | null {
  if (event.isComposing || event.metaKey) return null; // IME composition, OS/browser (Cmd) shortcuts
  const chord = chordOf(event);
  // In text inputs Ctrl+A / Ctrl+D are editing keys; Alt+←/→ remain.
  const editingKey = opts.textInput && chord.ctrl && !chord.alt && (chord.key === 'a' || chord.key === 'd');
  for (const binding of BINDINGS) {
    if (editingKey && (binding.action === 'tab.prev' || binding.action === 'tab.next')) {
      if (binding.keys.some(text => matches(cachedChord(text), chord))) continue;
    }
    if (applies(binding, context, opts) && bindingMatches(binding, chord)) return binding.action;
  }
  if (context === 'terminal') {
    // Every other key goes to the agent (term.typing, term.ctrl_c), matching
    // the TUI's ForwardToPty for a maximized container.
    if (['Shift', 'Control', 'Alt', 'Meta', 'CapsLock'].includes(event.key)) return null;
    return 'terminal.forward';
  }
  return null;
}

export interface ShortcutSheetEntry {
  group: Binding['group'];
  label: string;
  keys: readonly string[];
  alternates: readonly string[];
  reserved: readonly string[];
  buttonOnly: boolean;
}

/** Data for the in-app shortcut list (matrix keys.shortcut_list / keys.reserved). */
export function shortcutSheet(): ShortcutSheetEntry[] {
  const seen = new Set<string>();
  const out: ShortcutSheetEntry[] = [];
  for (const binding of [...GLOBAL_BINDINGS, ...CONTEXT_BINDINGS, ...DIALOG_BINDINGS]) {
    const id = `${binding.group}|${binding.label}|${binding.keys.join(',')}`;
    if (seen.has(id)) continue;
    seen.add(id);
    out.push({
      group: binding.group,
      label: binding.label,
      keys: binding.keys,
      alternates: binding.alternates ?? [],
      reserved: binding.reserved ?? [],
      buttonOnly: !!binding.buttonOnly,
    });
  }
  return out;
}

/** True when a chord is one the browser usually keeps (show the alternate instead). */
export function isReservedChord(text: string): boolean {
  return BINDINGS.some(binding => binding.reserved?.includes(text));
}
