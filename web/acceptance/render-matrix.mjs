#!/usr/bin/env node
// Renders action-matrix.md from action-matrix.json. The JSON is the hand-maintained
// source; the Markdown is generated and must not be edited by hand.
//
//   node web/acceptance/render-matrix.mjs            # rewrite action-matrix.md
//   node web/acceptance/render-matrix.mjs --check    # exit 1 if the .md is stale
//
// Both modes validate the JSON first: every row needs a user goal and a web
// entry point, and a row whose route is missing, shortcut-only or moving must
// say what has to change. A shortcut may accelerate an action; it is never an
// entry point.
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const jsonPath = join(here, 'action-matrix.json');
const mdPath = join(here, 'action-matrix.md');
const matrix = JSON.parse(readFileSync(jsonPath, 'utf8'));

const cell = text => String(text).replaceAll('|', '\\|');

/** Entry states that leave work for the UI steps; each needs a `gap` note. */
const OPEN_STATES = ['missing', 'shortcut_only', 'relocate'];

function validate() {
  const errors = [];
  const enums = matrix.fields ?? {};
  const oneOf = (name, value, where) => {
    if (!(enums[name] ?? []).includes(value)) errors.push(`${where}: ${name} '${value}' is not one of ${(enums[name] ?? []).join(', ')}`);
  };
  const text = value => typeof value === 'string' && value.trim() !== '';
  const seen = new Set();
  for (const row of matrix.rows) {
    const where = row.id ?? '(row without id)';
    if (seen.has(row.id)) errors.push(`${where}: duplicate id`);
    seen.add(row.id);
    oneOf('daemon.kind', row.daemon?.kind, where);
    oneOf('browser.availability', row.browser?.availability, where);
    oneOf('status', row.status, where);
    oneOf('parity', row.parity, where);
    if (!text(row.goal)) errors.push(`${where}: missing user goal`);
    const entry = row.web_entry;
    if (!entry) {
      errors.push(`${where}: missing web_entry`);
      continue;
    }
    oneOf('web_entry.surface', entry.surface, where);
    oneOf('web_entry.state', entry.state, where);
    for (const also of entry.also ?? []) oneOf('web_entry.surface', also, where);
    if (!text(entry.location)) errors.push(`${where}: web_entry.location is empty`);
    if (OPEN_STATES.includes(entry.state) && !text(entry.gap)) errors.push(`${where}: web_entry.state '${entry.state}' needs a gap note`);
    if ((entry.state === 'unavailable') !== (row.browser.availability === 'unavailable')) errors.push(`${where}: web_entry.state and browser.availability disagree about 'unavailable'`);
    if (row.browser.availability === 'unavailable' && !text(row.browser.reason)) errors.push(`${where}: unavailable without a reason`);
  }
  return errors;
}

const errors = validate();
if (errors.length) {
  console.error(`action-matrix.json is invalid (${errors.length}):\n  ${errors.join('\n  ')}`);
  process.exit(1);
}

function browserCell(browser) {
  if (browser.availability === 'unavailable') return `unavailable: ${browser.reason} — ${browser.control}`;
  let out = browser.control;
  if (browser.shortcut) out += ` \`${browser.shortcut}\``;
  if (browser.alt_shortcut) out += ` (alt \`${browser.alt_shortcut}\`)`;
  if (browser.availability === 'web_only') out += ' [web only]';
  return out;
}

function entryCell(entry) {
  let out = `${entry.surface}: ${entry.location}`;
  if (entry.also?.length) out += ` (also ${entry.also.join(', ')})`;
  return out;
}

const lines = [
  '# webapp — TUI screen/action acceptance matrix',
  '',
  `Rendered from \`action-matrix.json\` (schema version ${matrix.schema_version}); do not edit by hand. One row per TUI screen/action. \`daemon\` names the engine operation (awman.frontend.v1 frame, awman.terminal.v1 frame, HTTP route, or local view state); \`browser\` names the control and shortcut that exist today (or why the action is unavailable).`,
  '',
  'Legend: parity `engine_trace` = identical engine-side outcome for the scripted action on TUI and web; `wire_frame` = the exact frame is asserted; `view_only` = rendered from the same snapshot; `web_only` = no TUI counterpart.',
  '',
  '`user goal` is what the person is trying to do. `web entry point` is the surface and region that must offer the action without a keyboard shortcut. `entry` is its state: `present` = a visible route exists and stays; `relocate` = a route exists but moves; `shortcut_only` = only a key reaches it today; `missing` = no route today; `unavailable` = deliberately not offered in the browser. Shortcuts may accelerate an action but are never its entry point.',
  '',
];

const screens = [...new Set(matrix.rows.map(row => row.screen))];
for (const screen of screens) {
  lines.push('', `## ${screen}`, '', '| id | action | user goal | TUI key | daemon operation | browser control / shortcut | web entry point | entry | capability | feature | status | parity |', '|---|---|---|---|---|---|---|---|---|---|---|---|');
  for (const row of matrix.rows.filter(r => r.screen === screen)) {
    lines.push(`| \`${row.id}\` | ${cell(row.title)} | ${cell(row.goal)} | ${cell(row.tui.key)} | ${cell(`${row.daemon.kind}: ${row.daemon.detail}`)} | ${cell(browserCell(row.browser))} | ${cell(entryCell(row.web_entry))} | ${row.web_entry.state} | \`${row.capability}\` | ${row.feature} | ${row.status} | ${row.parity} |`);
  }
}

const headings = {
  missing: 'No web route today',
  shortcut_only: 'Reachable only by a shortcut today',
  relocate: 'Route exists but moves',
};
lines.push('', '## Web entry points to add or move');
for (const state of OPEN_STATES) {
  const rows = matrix.rows.filter(row => row.web_entry.state === state);
  lines.push('', `### ${headings[state]} (\`${state}\`: ${rows.length})`, '');
  if (!rows.length) {
    lines.push('None.');
    continue;
  }
  lines.push('| id | action | web entry point | what has to change |', '|---|---|---|---|');
  for (const row of rows) lines.push(`| \`${row.id}\` | ${cell(row.title)} | ${cell(entryCell(row.web_entry))} | ${cell(row.web_entry.gap)} |`);
}

function countTable(title, label, key) {
  const counts = {};
  for (const row of matrix.rows) counts[key(row)] = (counts[key(row)] ?? 0) + 1;
  lines.push('', `## ${title}`, '', `| ${label} | rows |`, '|---|---|');
  for (const name of Object.keys(counts).sort()) lines.push(`| ${name} | ${counts[name]} |`);
  lines.push(`| total | ${matrix.rows.length} |`);
}
countTable('Counts', 'status', row => row.status);
countTable('Entry-point counts', 'entry', row => row.web_entry.state);
lines.push('');

const rendered = lines.join('\n');
if (process.argv.includes('--check')) {
  if (readFileSync(mdPath, 'utf8') !== rendered) {
    console.error('action-matrix.md is stale; run node web/acceptance/render-matrix.mjs');
    process.exit(1);
  }
  console.log('action-matrix.md is up to date');
} else {
  writeFileSync(mdPath, rendered);
  console.log(`rendered ${matrix.rows.length} rows to action-matrix.md`);
}
