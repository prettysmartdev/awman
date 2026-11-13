// Command-box completion over the daemon's catalogue projection (design
// §2.9). Port of CommandCatalogue::tui_completions
// (src/command/dispatch/projections/tui_hints.rs): string matching only —
// parsing, defaults and validation stay in the engine. The catalogue served
// to browsers is already the TUI-visible projection, so no visibility filter
// is applied here.
import type { Catalogue, CatalogueCommand } from './types';

export interface Completion {
  completion: string;
  help: string;
}

function nameOf(command: CatalogueCommand): string {
  return command.path[command.path.length - 1] ?? '';
}

function findSubcommand(children: readonly CatalogueCommand[], token: string): CatalogueCommand | undefined {
  return children.find(child => nameOf(child) === token || child.aliases.includes(token));
}

/** Completions for a partial command-box input (e.g. "ex", "exec wo", "chat --"). */
export function completions(catalogue: Pick<Catalogue, 'commands'>, partial: string): Completion[] {
  const trimmed = partial.trim();
  const parts = trimmed === '' ? [] : trimmed.split(/\s+/);
  let children: readonly CatalogueCommand[] = catalogue.commands;
  let flags: CatalogueCommand['flags'] = [];
  let consumed = 0;
  for (let index = 0; index < parts.length; index++) {
    const part = parts[index] ?? '';
    // A token that is not a subcommand (typically the last, partial one) stops the walk.
    const sub = findSubcommand(children, part);
    if (sub === undefined) break;
    children = sub.subcommands;
    flags = sub.flags;
    consumed = index + 1;
  }
  const prefix = parts[consumed] ?? '';
  const out: Completion[] = [];
  for (const child of children) {
    if (nameOf(child).startsWith(prefix)) out.push({ completion: nameOf(child), help: child.help });
  }
  for (const flag of flags) {
    const candidate = `--${flag.long}`;
    if (candidate.startsWith(prefix)) out.push({ completion: candidate, help: flag.help });
  }
  return out;
}

/** The TUI fills the box with the first (Tab) or last (Shift+Tab) suggestion. */
export function completionFor(catalogue: Pick<Catalogue, 'commands'>, partial: string, direction: 'next' | 'prev'): string | null {
  const list = completions(catalogue, partial);
  const pick = direction === 'next' ? list[0] : list[list.length - 1];
  return pick ? pick.completion : null;
}
