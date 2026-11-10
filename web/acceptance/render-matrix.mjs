#!/usr/bin/env node
// Renders action-matrix.md from action-matrix.json. The JSON is the hand-maintained
// source; the Markdown is generated and must not be edited by hand.
//
//   node web/acceptance/render-matrix.mjs            # rewrite action-matrix.md
//   node web/acceptance/render-matrix.mjs --check    # exit 1 if the .md is stale
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const jsonPath = join(here, 'action-matrix.json');
const mdPath = join(here, 'action-matrix.md');
const matrix = JSON.parse(readFileSync(jsonPath, 'utf8'));

const cell = text => String(text).replaceAll('|', '\\|');

function browserCell(browser) {
  if (browser.availability === 'unavailable') return `unavailable: ${browser.reason} — ${browser.control}`;
  let out = browser.control;
  if (browser.shortcut) out += ` \`${browser.shortcut}\``;
  if (browser.alt_shortcut) out += ` (alt \`${browser.alt_shortcut}\`)`;
  if (browser.availability === 'web_only') out += ' [web only]';
  return out;
}

const lines = [
  '# webapp v0 — TUI screen/action acceptance matrix',
  '',
  `Rendered from \`action-matrix.json\` (schema version ${matrix.schema_version}); do not edit by hand. One row per TUI screen/action. \`daemon\` names the engine operation (awman.frontend.v1 frame, awman.terminal.v1 frame, HTTP route, or local view state); \`browser\` names the control and shortcut (or why it is unavailable). \`status: pending_dependency\` rows describe the 0143 surface the webapp must present once 0143 lands; the parity harness skips them while their capability is absent from \`GET /v1/status\`.`,
  '',
  'Legend: parity `engine_trace` = identical engine-side outcome for the scripted action on TUI and web; `wire_frame` = the exact frame is asserted; `view_only` = rendered from the same snapshot; `web_only` = no TUI counterpart.',
  '',
];

const screens = [...new Set(matrix.rows.map(row => row.screen))];
for (const screen of screens) {
  lines.push('', `## ${screen}`, '', '| id | action | TUI key | daemon operation | browser control / shortcut | capability | feature | status | parity |', '|---|---|---|---|---|---|---|---|---|');
  for (const row of matrix.rows.filter(r => r.screen === screen)) {
    lines.push(`| \`${row.id}\` | ${cell(row.title)} | ${cell(row.tui.key)} | ${cell(`${row.daemon.kind}: ${row.daemon.detail}`)} | ${cell(browserCell(row.browser))} | \`${row.capability}\` | ${row.feature} | ${row.status} | ${row.parity} |`);
  }
}

const counts = {};
for (const row of matrix.rows) counts[row.status] = (counts[row.status] ?? 0) + 1;
lines.push('', '## Counts', '', '| status | rows |', '|---|---|');
for (const status of Object.keys(counts).sort()) lines.push(`| ${status} | ${counts[status]} |`);
lines.push(`| total | ${matrix.rows.length} |`, '');

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
