// CSP-safe DOM renderer for a wterm TerminalCore.
//
// Every row is a <div> of <span> runs built with createElement/textContent:
// agent output never becomes markup. Attributes are class names (styled by
// TerminalView.module.css); explicit colours are set through CSSOM
// (`element.style.color`), which the production CSP (`style-src 'self'`)
// allows, unlike `style="…"` markup. OSC 8 cells are only underlined — the
// renderer never creates anchors; vetted targets go to the link list.
import type { CellData, TerminalCore, TerminalThemeColors } from '@wterm/core';
import { findUrls, vetLink } from '../security/sanitize';

const FLAG_BOLD = 0x01;
const FLAG_DIM = 0x02;
const FLAG_ITALIC = 0x04;
const FLAG_UNDERLINE = 0x08;
const FLAG_REVERSE = 0x20;
const FLAG_INVISIBLE = 0x40;
const FLAG_STRIKE = 0x80;
const DEFAULT_COLOR = 256;

export interface Run {
  text: string;
  className: string;
  fg: string | null;
  bg: string | null;
  /** Vetted OSC 8 target (http/https only), or null. */
  link: string | null;
}

export interface RowModel {
  runs: Run[];
  text: string;
  /** Vetted OSC 8 links in this row: target + visible text. */
  links: { url: string; text: string }[];
  key: string;
}

export interface RenderOptions {
  /** Lines scrolled up into history (0 = live screen). */
  viewportOffset: number;
  theme: TerminalThemeColors;
}

function rgb(packed: number): string {
  return `rgb(${(packed >> 16) & 0xff},${(packed >> 8) & 0xff},${packed & 0xff})`;
}

/** xterm 256-colour palette for indices 16-255 (cores without resolved RGB). */
function paletteRgb(index: number): number {
  if (index < 232) {
    const n = index - 16;
    return ((Math.floor(n / 36) * 51) << 16) | ((Math.floor(n / 6) % 6) * 51) << 8 | ((n % 6) * 51);
  }
  const level = (index - 232) * 10 + 8;
  return (level << 16) | (level << 8) | level;
}

function colour(index: number, resolved: number | undefined, theme: TerminalThemeColors): number | null {
  if (resolved !== undefined) return resolved;
  if (index === DEFAULT_COLOR || index < 0 || index > 255) return null;
  return index < 16 ? (theme.palette[index] ?? null) : paletteRgb(index);
}

function cellText(cell: CellData): string {
  if (cell.chars) return cell.chars;
  if (!cell.char || cell.char < 0x20) return ' ';
  try {
    return String.fromCodePoint(cell.char);
  } catch {
    return ' ';
  }
}

function attrClass(cell: CellData, wide: boolean, cursor: boolean): string {
  const f = cell.flags;
  const names: string[] = [];
  if (f & FLAG_BOLD) names.push('tb');
  if (f & FLAG_DIM) names.push('td');
  if (f & FLAG_ITALIC) names.push('ti');
  if (cell.underlineStyle && cell.underlineStyle !== 'none') names.push('tu', `tu-${cell.underlineStyle}`);
  else if (f & FLAG_UNDERLINE) names.push('tu');
  if (f & FLAG_STRIKE) names.push('ts');
  if (f & FLAG_INVISIBLE) names.push('th');
  if (cell.linkUri) names.push('tl');
  if (wide) names.push('tw');
  if (cursor) names.push('tc');
  return names.join(' ');
}

/** Build a row model from a cell accessor (live row or history line). */
export function buildRow(cols: number, getCell: (col: number) => CellData, cursorCol: number | null, theme: TerminalThemeColors): RowModel {
  const runs: Run[] = [];
  const links: { url: string; text: string }[] = [];
  let text = '';
  let linkRun: { url: string; text: string; key: string } | null = null;
  for (let col = 0; col < cols; col++) {
    const cell = getCell(col);
    if (cell.width === 0) continue; // continuation of a wide cell
    const wide = (cell.width ?? 1) >= 2;
    const cursor = cursorCol === col;
    const chars = cell.spacerHead ? ' ' : cellText(cell);
    let fg = colour(cell.fg, cell.fgRgb, theme);
    let bg = colour(cell.bg, cell.bgRgb, theme);
    if (cell.flags & FLAG_REVERSE) [fg, bg] = [bg ?? theme.background, fg ?? theme.foreground];
    const link = cell.linkUri ? vetLink(cell.linkUri) : null;
    const run: Run = { text: chars, className: attrClass(cell, wide, cursor), fg: fg === null ? null : rgb(fg), bg: bg === null ? null : rgb(bg), link };
    const last = runs[runs.length - 1];
    if (!wide && !cursor && last && !last.className.includes('tw') && !last.className.includes('tc') && last.className === run.className && last.fg === run.fg && last.bg === run.bg && last.link === run.link) {
      last.text += chars;
    } else {
      runs.push(run);
    }
    text += chars;
    const key = cell.linkKey ?? cell.linkUri ?? '';
    if (link) {
      if (linkRun && linkRun.key === key) linkRun.text += chars;
      else {
        linkRun = { url: link, text: chars, key };
        links.push(linkRun);
      }
    } else linkRun = null;
  }
  const merged = links.map(l => ({ url: l.url, text: l.text.trim() || l.url }));
  return { runs, text, links: merged, key: JSON.stringify(runs) };
}

/** Rendered text of every visible row (for tests and selection fallbacks). */
export function rowsText(rows: readonly RowModel[]): string[] {
  return rows.map(r => r.text.trimEnd());
}

export interface Viewport {
  rows: RowModel[];
  /** History lines available above the live screen. */
  historyLines: number;
}

/** Read the visible window (history + live screen) from the core. */
export function readViewport(core: TerminalCore, opts: RenderOptions): Viewport {
  const cols = core.getCols();
  const rows = core.getRows();
  const history = core.usingAltScreen() ? 0 : core.getScrollbackCount();
  const offset = Math.max(0, Math.min(opts.viewportOffset, history));
  const cursor = core.getCursor();
  const out: RowModel[] = [];
  for (let i = 0; i < rows; i++) {
    const logical = history - offset + i; // 0 = oldest history line
    if (logical < history) {
      const back = history - 1 - logical; // 0 = newest history line
      const len = core.getScrollbackLineLen(back);
      out.push(buildRow(cols, col => (col < len ? core.getScrollbackCell(back, col) : BLANK), null, opts.theme));
    } else {
      const row = logical - history;
      const cursorCol = offset === 0 && cursor.visible && cursor.row === row ? cursor.col : null;
      out.push(buildRow(cols, col => core.getCell(row, col), cursorCol, opts.theme));
    }
  }
  return { rows: out, historyLines: history };
}

const BLANK: CellData = { char: 32, fg: DEFAULT_COLOR, bg: DEFAULT_COLOR, flags: 0, width: 1 };

/** Every vetted link in the viewport: OSC 8 targets first, then plain URLs. */
export function viewportLinks(rows: readonly RowModel[]): { url: string; text: string; source: 'osc8' | 'text' }[] {
  const seen = new Set<string>();
  const out: { url: string; text: string; source: 'osc8' | 'text' }[] = [];
  for (const row of rows) {
    for (const link of row.links) {
      if (seen.has(`o${link.url}`)) continue;
      seen.add(`o${link.url}`);
      out.push({ url: link.url, text: link.text, source: 'osc8' });
    }
  }
  for (const row of rows) {
    for (const url of findUrls(row.text)) {
      if (seen.has(`o${url}`) || seen.has(`t${url}`)) continue;
      seen.add(`t${url}`);
      out.push({ url, text: url, source: 'text' });
    }
  }
  return out;
}

/** Paints row models into a container, rebuilding only rows that changed. */
export class DomRenderer {
  private rowEls: HTMLDivElement[] = [];
  private keys: string[] = [];

  constructor(private readonly container: HTMLElement) {}

  paint(rows: readonly RowModel[]): void {
    const doc = this.container.ownerDocument;
    while (this.rowEls.length > rows.length) {
      this.rowEls.pop()?.remove();
      this.keys.pop();
    }
    while (this.rowEls.length < rows.length) {
      const el = doc.createElement('div');
      el.className = 'term-row';
      this.container.appendChild(el);
      this.rowEls.push(el);
      this.keys.push('');
    }
    rows.forEach((row, index) => {
      const el = this.rowEls[index];
      if (!el || this.keys[index] === row.key) return;
      this.keys[index] = row.key;
      el.replaceChildren(...row.runs.map(run => span(doc, run)));
    });
  }

  clear(): void {
    for (const el of this.rowEls) el.remove();
    this.rowEls = [];
    this.keys = [];
  }
}

function span(doc: Document, run: Run): HTMLSpanElement {
  const el = doc.createElement('span');
  if (run.className) el.className = run.className;
  // CSSOM, not markup: permitted under style-src 'self'.
  if (run.fg) el.style.color = run.fg;
  if (run.bg) el.style.backgroundColor = run.bg;
  el.textContent = run.text;
  return el;
}
