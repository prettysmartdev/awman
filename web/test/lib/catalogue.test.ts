import { describe, expect, it } from 'vitest';
import { completionFor, completions } from '@/lib/client/catalogue';
import type { Catalogue, CatalogueCommand } from '@/lib/client/types';

const cmd = (path: string[], extra: Partial<CatalogueCommand> = {}): CatalogueCommand => ({
  path,
  help: `${path.join(' ')} help`,
  long_help: null,
  aliases: [],
  flags: [],
  arguments: [],
  subcommands: [],
  ...extra,
});

const CATALOGUE: Pick<Catalogue, 'commands'> = {
  commands: [
    cmd(['chat'], { flags: [{ long: 'yolo', short: null, help: 'unattended', kind: 'bool', default: null }, { long: 'model', short: 'm', help: 'model', kind: 'string', default: null }] }),
    cmd(['clean']),
    cmd(['exec'], { aliases: ['x'], subcommands: [cmd(['exec', 'workflow'], { flags: [{ long: 'yolo', short: null, help: 'y', kind: 'bool', default: null }] }), cmd(['exec', 'prompt'])] }),
  ],
};

describe('command-box completion (tui_completions port)', () => {
  it('completes top-level commands by prefix', () => {
    expect(completions(CATALOGUE, 'c').map(c => c.completion)).toEqual(['chat', 'clean']);
    expect(completions(CATALOGUE, '').map(c => c.completion)).toEqual(['chat', 'clean', 'exec']);
    expect(completions(CATALOGUE, 'zz')).toEqual([]);
  });

  it('descends into subcommands (and aliases) and lists their flags', () => {
    expect(completions(CATALOGUE, 'exec wo').map(c => c.completion)).toEqual(['workflow']);
    expect(completions(CATALOGUE, 'x ').map(c => c.completion)).toEqual(['workflow', 'prompt']);
    expect(completions(CATALOGUE, 'chat --').map(c => c.completion)).toEqual(['--yolo', '--model']);
    expect(completions(CATALOGUE, 'exec workflow --y').map(c => c.completion)).toEqual(['--yolo']);
  });

  it('Tab takes the first suggestion, Shift+Tab the last (the TUI replaces the input)', () => {
    expect(completionFor(CATALOGUE, 'c', 'next')).toBe('chat');
    expect(completionFor(CATALOGUE, 'c', 'prev')).toBe('clean');
    expect(completionFor(CATALOGUE, 'zz', 'next')).toBeNull();
  });
});
