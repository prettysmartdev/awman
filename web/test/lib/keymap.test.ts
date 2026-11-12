import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { BINDINGS, map, parseChord, shortcutSheet, type Binding, type KeyContext, type KeyEventLike, type MapOptions } from '@/lib/keys/keymap';

interface MatrixRow {
  id: string;
  browser: { shortcut: string | null; alt_shortcut: string | null };
}

const matrix = JSON.parse(readFileSync(join(__dirname, '../../acceptance/action-matrix.json'), 'utf8')) as {
  rows: MatrixRow[];
  reserved_browser_shortcuts: Record<string, string | null>;
};

const KEY_TOKEN = /^(?:(?:Ctrl|Alt|Shift)\+)*(?:[A-Za-z0-9,?\\]|Esc|Enter|Tab|Insert|PgUp|PgDn|Home|End|[↑↓←→])$/;

/** Chords named in a free-form matrix shortcut cell ('↑/↓ PgUp/PgDn b/e', 'g / c / Esc', 'keys / Esc'). */
function chordsIn(cell: string): string[] {
  return cell
    .split(/\s+|(?<!\+)\/(?!$)/)
    .map(token => token.trim())
    .filter(token => token && token !== '/' && KEY_TOKEN.test(token));
}

function eventFor(chord: string): KeyEventLike {
  const parsed = parseChord(chord);
  const letter = /^[a-z]$/.test(parsed.key) ? `Key${parsed.key.toUpperCase()}` : undefined;
  return {
    key: parsed.shift && parsed.key.length === 1 ? parsed.key.toUpperCase() : parsed.key,
    code: letter ?? (parsed.key === '\\' ? 'Backslash' : parsed.key === ',' ? 'Comma' : undefined),
    ctrlKey: parsed.ctrl,
    altKey: parsed.alt,
    shiftKey: parsed.shift,
    metaKey: false,
  };
}

/** Options that make a binding's `when` condition true. */
function optsFor(binding: Binding, context: KeyContext): MapOptions {
  return {
    ...(binding.dialogScopes && context === 'dialog' ? { dialog: binding.dialogScopes[0] } : context === 'dialog' ? { dialog: 'other' } : {}),
    composerEmpty: true,
    overviewHScroll: true,
    containers: 2,
    watching: binding.action === 'exec.stopWatch',
  };
}

function resolvesForRow(rowId: string, chord: string): boolean {
  const bindings = BINDINGS.filter(binding => binding.matrix.includes(rowId));
  return bindings.some(binding =>
    binding.contexts.some(context => {
      const opts = optsFor(binding, context);
      return map(eventFor(chord), context, opts) === binding.action && [...binding.keys, ...(binding.alternates ?? [])].includes(chord);
    }),
  );
}

describe('keymap vs the action matrix', () => {
  const rows = matrix.rows.filter(row => row.browser.shortcut);

  it('covers every matrix row that names a browser shortcut', () => {
    const rowsWithChords = rows.filter(row => chordsIn(row.browser.shortcut ?? '').length > 0);
    const missing = rowsWithChords.filter(row => !BINDINGS.some(binding => binding.matrix.includes(row.id))).map(row => row.id);
    expect(missing).toEqual([]);
    expect(rowsWithChords.length).toBeGreaterThan(80);
  });

  for (const row of rows) {
    const chords = [...chordsIn(row.browser.shortcut ?? ''), ...chordsIn(row.browser.alt_shortcut ?? '')];
    if (!chords.length) continue;
    it(`${row.id}: ${row.browser.shortcut}${row.browser.alt_shortcut ? ` (alt ${row.browser.alt_shortcut})` : ''}`, () => {
      const unresolved = chords.filter(chord => !resolvesForRow(row.id, chord));
      expect(unresolved).toEqual([]);
    });
  }

  it('gives every reserved combination its matrix alternate', () => {
    for (const [reserved, alternate] of Object.entries(matrix.reserved_browser_shortcuts)) {
      const binding = BINDINGS.find(b => b.reserved?.includes(reserved));
      expect(binding, reserved).toBeDefined();
      if (alternate) expect(binding?.alternates).toContain(alternate);
      else expect(binding?.buttonOnly).toBe(true);
    }
  });
});

describe('keymap behaviour (keymap.rs parity)', () => {
  const ev = (chord: string, extra: Partial<KeyEventLike> = {}) => ({ ...eventFor(chord), ...extra });

  it('maps global chords from any non-dialog context', () => {
    for (const context of ['composer', 'execution', 'squadList', 'global'] as const) {
      expect(map(ev('Ctrl+G'), context)).toBe('git.toggle');
      expect(map(ev('Ctrl+O'), context)).toBe('workflow.overview');
      expect(map(ev('Alt+T'), context)).toBe('tab.new');
      expect(map(ev('Ctrl+\\'), context)).toBe('containers.detach');
    }
  });

  it('suppresses tab switching and overview inside dialogs (TUI-2)', () => {
    expect(map(ev('Ctrl+A'), 'dialog', { dialog: 'yes_no' })).toBeNull();
    expect(map(ev('Ctrl+O'), 'dialog', { dialog: 'yes_no' })).toBeNull();
    expect(map(ev('Ctrl+A'), 'dialog', { dialog: 'yolo' })).toBe('tab.prev');
  });

  it('keeps Ctrl+A/Ctrl+D as editing keys in text inputs; Alt+arrows still switch tabs', () => {
    expect(map(ev('Ctrl+A'), 'composer', { textInput: true })).toBeNull();
    expect(map(ev('Alt+←'), 'composer', { textInput: true })).toBe('tab.prev');
    expect(map(ev('Ctrl+A'), 'execution')).toBe('tab.prev');
  });

  it('forwards everything else to a focused terminal, including Ctrl+C', () => {
    expect(map(ev('Ctrl+C'), 'terminal')).toBe('terminal.forward');
    expect(map(ev('x'), 'terminal')).toBe('terminal.forward');
    expect(map(ev('Esc'), 'terminal')).toBe('terminal.forward');
    expect(map(ev('Ctrl+G'), 'terminal')).toBe('git.toggle');
    expect(map(ev('Ctrl+S'), 'terminal', { containers: 1 })).toBe('terminal.forward');
    expect(map(ev('Ctrl+S'), 'terminal', { containers: 2 })).toBe('container.focusNextSlot');
    expect(map({ key: 'Shift', ctrlKey: false, altKey: false, shiftKey: true, metaKey: false }, 'terminal')).toBeNull();
  });

  it('Ctrl+C elsewhere is the cancel / close / quit interrupt', () => {
    expect(map(ev('Ctrl+C'), 'composer')).toBe('interrupt');
    expect(map(ev('Ctrl+C'), 'dialog', { dialog: 'quit_confirm' })).toBe('dialog.quit');
    expect(map(ev('Ctrl+C'), 'dialog', { dialog: 'control_board' })).toBe('wfc.abort');
  });

  it('q quits only from an empty command box', () => {
    expect(map(ev('q'), 'composer', { composerEmpty: true })).toBe('composer.quit');
    expect(map(ev('q'), 'composer', { composerEmpty: false })).toBeNull();
  });

  it('uses the physical key for Alt chords (macOS Alt+T yields "†")', () => {
    expect(map({ key: '†', code: 'KeyT', ctrlKey: false, altKey: true, shiftKey: false, metaKey: false }, 'composer')).toBe('tab.new');
  });

  it('ignores Cmd chords and IME composition', () => {
    expect(map(ev('Ctrl+G', { metaKey: true }), 'composer')).toBeNull();
    expect(map(ev('Enter', { isComposing: true }), 'composer')).toBeNull();
  });

  it('builds the shortcut sheet with alternates and reserved keys', () => {
    const sheet = shortcutSheet();
    const newTab = sheet.find(entry => entry.label === 'New tab');
    expect(newTab).toMatchObject({ keys: ['Ctrl+T'], alternates: ['Alt+T'], reserved: ['Ctrl+T'] });
    expect(sheet.some(entry => entry.buttonOnly && entry.reserved.includes('Ctrl+Shift+C'))).toBe(true);
  });
});

describe('terminal keyboard exit (B7)', () => {
  it('maps Ctrl+] to terminal.exit in the terminal and lists it on the shortcut sheet', () => {
    expect(map({ key: ']', code: 'BracketRight', ctrlKey: true, altKey: false, shiftKey: false, metaKey: false }, 'terminal')).toBe('terminal.exit');
    expect(map({ key: 'Tab', code: 'Tab', ctrlKey: false, altKey: false, shiftKey: false, metaKey: false }, 'terminal')).toBe('terminal.forward');
    expect(shortcutSheet().some(entry => entry.keys.includes('Ctrl+]') && entry.group === 'Terminal')).toBe(true);
  });
});
