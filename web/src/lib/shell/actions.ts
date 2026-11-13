import type { Catalogue, CatalogueCommand } from '@/lib/client/types';
import type { WebAction } from '@/lib/keys/keymap';

/** Shared by pointer surfaces and keyboard dispatch. Reasons stay visible. */
export interface ShellAction {
  id: string;
  label: string;
  group: 'Go to' | 'Agent' | 'Workflow' | 'Workspace' | 'Output' | 'Run' | 'Danger';
  matrix: readonly string[];
  shortcut?: WebAction;
  description?: string;
  disabled?: string;
  /**
   * False for a keyboard accelerator of a control that already has its own
   * visible entry point (scrolling a list): it still dispatches its shortcut
   * and appears in the shortcut list, but not in the Actions sheet.
   */
  menu?: false;
  run(): void;
}
export const ACTION_GROUPS: ShellAction['group'][] = ['Go to', 'Workspace', 'Agent', 'Workflow', 'Output', 'Run', 'Danger'];

export function invokeAction(action: ShellAction): void { if (!action.disabled) action.run(); }

/** The projection omits runnable metadata. These grouping-only parents are
 * documented by the catalogue; other parents (squad / queue) are runnable. */
const GROUP_ONLY = new Set(['specs', 'agents', 'exec', 'daemon', 'remote', 'remote session', 'remote exec', 'new']);
export function catalogueActions(catalogue: Catalogue, fill: (text: string) => void, disabled?: string): ShellAction[] {
  const walk = (commands: CatalogueCommand[]): ShellAction[] => commands.flatMap(command => {
    const path = command.path.join(' ');
    const children = walk(command.subcommands);
    if (GROUP_ONLY.has(path)) return children;
    return [{ id: `run:${path}`, label: path, group: path === 'daemon stop' ? 'Danger' as const : 'Run' as const,
      matrix: ['nav.command_menu_catalogue'], description: command.help, disabled,
      run: () => fill(path + ' '),
    }, ...children];
  });
  return walk(catalogue.commands);
}

/** Adapter for the existing controlled Composer, without submitting or
 * replacing its validation. The surfaces step can replace with a draft prop. */
export function fillComposer(text: string): void {
  const input = document.getElementById('command');
  if (!(input instanceof HTMLTextAreaElement) || input.readOnly) return;
  Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set?.call(input, text);
  input.dispatchEvent(new Event('input', { bubbles: true }));
  input.focus();
  input.setSelectionRange(text.length, text.length);
}
