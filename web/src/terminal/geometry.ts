/** Geometry is local to a terminal: never measure the page, tab or screen grid. */
export interface GridSize { cols: number; rows: number }
export interface CellMetrics { width: number; height: number; cellWidth: number; cellHeight: number }

export const CELL_PROBE_COLUMNS = 10;

export function gridFromMetrics({ width, height, cellWidth, cellHeight }: CellMetrics): GridSize | null {
  if (![width, height, cellWidth, cellHeight].every(n => Number.isFinite(n) && n > 0)) return null;
  const cols = Math.floor(width / cellWidth);
  const rows = Math.floor(height / cellHeight);
  // A hidden/collapsed view must never shrink the shared PTY to a made-up minimum.
  if (cols < 2 || rows < 2) return null;
  return { cols: Math.min(1000, cols), rows: Math.min(1000, rows) };
}

/** The probe inherits the exact screen font and has one rendered row's height. */
export function measureTerminalGrid(viewport: HTMLElement, probe: HTMLElement): GridSize | null {
  const win = viewport.ownerDocument.defaultView;
  if (!win || !viewport.isConnected || viewport.getClientRects().length === 0) return null;
  const css = win.getComputedStyle(viewport);
  if (css.visibility === 'hidden' || css.visibility === 'collapse') return null;
  const rect = viewport.getBoundingClientRect();
  const cell = probe.getBoundingClientRect();
  // client sizes exclude borders/scrollbars; apply the rendered scale to both
  // viewport and probe so CSS zoom/transforms do not mix coordinate spaces.
  const scaleX = viewport.offsetWidth ? rect.width / viewport.offsetWidth : 1;
  const scaleY = viewport.offsetHeight ? rect.height / viewport.offsetHeight : 1;
  return gridFromMetrics({
    width: (viewport.clientWidth - parseFloat(css.paddingLeft || '0') - parseFloat(css.paddingRight || '0')) * scaleX,
    height: (viewport.clientHeight - parseFloat(css.paddingTop || '0') - parseFloat(css.paddingBottom || '0')) * scaleY,
    cellWidth: cell.width / CELL_PROBE_COLUMNS,
    cellHeight: cell.height,
  });
}

/** Observe both available space AND glyph metrics. Returns complete teardown. */
export function observeTerminalGeometry(viewport: HTMLElement, probe: HTMLElement, fit: () => void): () => void {
  const win = viewport.ownerDocument.defaultView;
  if (!win) return () => undefined;
  let disposed = false;
  const recompute = () => { if (!disposed) fit(); };
  const resize = typeof ResizeObserver === 'function' ? new ResizeObserver(recompute) : null;
  resize?.observe(viewport);
  resize?.observe(probe);
  // Intersection changes also cover a retained tab/panel becoming visible.
  const intersection = typeof IntersectionObserver === 'function' ? new IntersectionObserver(recompute) : null;
  intersection?.observe(viewport);
  // visibility:hidden retains its box and need not trigger either observer.
  // Watch only ancestors' presentation attributes, never terminal output.
  const visibility = typeof MutationObserver === 'function' ? new MutationObserver(recompute) : null;
  for (let element: HTMLElement | null = viewport; element; element = element.parentElement) {
    visibility?.observe(element, { attributes: true, attributeFilter: ['class', 'style', 'hidden', 'open'] });
  }
  win.addEventListener('resize', recompute);
  win.addEventListener('pageshow', recompute);
  win.visualViewport?.addEventListener('resize', recompute);
  const doc = viewport.ownerDocument;
  doc.addEventListener('visibilitychange', recompute);
  doc.fonts?.addEventListener('loadingdone', recompute);
  doc.fonts?.addEventListener('loadingerror', recompute);
  void doc.fonts?.ready.then(recompute);
  recompute();
  return () => {
    disposed = true;
    resize?.disconnect();
    intersection?.disconnect();
    visibility?.disconnect();
    win.removeEventListener('resize', recompute);
    win.removeEventListener('pageshow', recompute);
    win.visualViewport?.removeEventListener('resize', recompute);
    doc.removeEventListener('visibilitychange', recompute);
    doc.fonts?.removeEventListener('loadingdone', recompute);
    doc.fonts?.removeEventListener('loadingerror', recompute);
  };
}

/** Height reserved for the agent header row above a terminal viewport. */
const AGENT_HEADER_PX = 48;
/** 13px monospace at line-height 1.3 when no rendered probe exists yet. */
const DEFAULT_CELL = { width: 7.8, height: 16.9 };
export const FALLBACK_PTY_SIZE: GridSize = { cols: 100, rows: 30 };

/**
 * Initial PTY size for a new command (the submit frame's pty_size): the grid
 * of the visible agent terminal when there is one, otherwise an estimate
 * from the workspace stage box. The holder's first fit() corrects it.
 */
export function estimatePtySize(doc: Document | undefined = typeof document === 'undefined' ? undefined : document): GridSize {
  if (!doc) return FALLBACK_PTY_SIZE;
  const probes = [...doc.querySelectorAll<HTMLElement>('[data-terminal-probe]')];
  for (const probe of probes) {
    const viewport = probe.parentElement;
    const size = viewport ? measureTerminalGrid(viewport, probe) : null;
    if (size) return size;
  }
  const cellProbe = probes.map(p => p.getBoundingClientRect()).find(r => r.width > 0 && r.height > 0);
  const cell = cellProbe ? { width: cellProbe.width / CELL_PROBE_COLUMNS, height: cellProbe.height } : DEFAULT_CELL;
  const stage = doc.querySelector<HTMLElement>('[aria-label="Workspace stage"]');
  const box = stage?.getBoundingClientRect();
  // Without a laid-out stage (hidden panel, no layout engine) there is nothing to measure.
  if (!box || box.width <= 0 || box.height <= 0) return FALLBACK_PTY_SIZE;
  const width = box.width - 32;
  const height = box.height - 32 - AGENT_HEADER_PX;
  const size = gridFromMetrics({ width, height, cellWidth: cell.width, cellHeight: cell.height });
  return size && size.cols >= 20 && size.rows >= 5 ? size : FALLBACK_PTY_SIZE;
}
