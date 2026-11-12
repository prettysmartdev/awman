// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { estimatePtySize, FALLBACK_PTY_SIZE, gridFromMetrics, measureTerminalGrid, observeTerminalGeometry } from '@/terminal/geometry';

afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

function elements() {
  const viewport = document.createElement('div');
  const probe = document.createElement('span');
  viewport.append(probe);
  document.body.append(viewport);
  viewport.style.visibility = 'visible';
  for (const [name, value] of Object.entries({ clientWidth: 803, clientHeight: 409, offsetWidth: 803, offsetHeight: 409 })) {
    Object.defineProperty(viewport, name, { configurable: true, value });
  }
  vi.spyOn(viewport, 'getClientRects').mockReturnValue([{}] as DOMRect[] as unknown as DOMRectList);
  vi.spyOn(viewport, 'getBoundingClientRect').mockReturnValue({ width: 803, height: 409 } as DOMRect);
  vi.spyOn(probe, 'getBoundingClientRect').mockReturnValue({ width: 78.25, height: 16.9 } as DOMRect);
  return { viewport, probe };
}

describe('terminal cell geometry', () => {
  it('floors real fractional cells and bounds allocations', () => {
    expect(gridFromMetrics({ width: 803, height: 409, cellWidth: 7.825, cellHeight: 16.9 })).toEqual({ cols: 102, rows: 24 });
    expect(gridFromMetrics({ width: 1e6, height: 1e6, cellWidth: 1, cellHeight: 1 })).toEqual({ cols: 1000, rows: 1000 });
  });
  it.each([0, -1, NaN, Infinity, -Infinity])('rejects invalid measurement %s in any field', value => {
    const valid = { width: 800, height: 400, cellWidth: 8, cellHeight: 16 };
    for (const key of Object.keys(valid)) expect(gridFromMetrics({ ...valid, [key]: value })).toBeNull();
  });
  it('never invents a minimum for a collapsed or sub-cell viewport', () => {
    expect(gridFromMetrics({ width: 8, height: 16, cellWidth: 8, cellHeight: 16 })).toBeNull();
  });
  it('measures only available content, including padding and rendered scaling', () => {
    const { viewport, probe } = elements();
    viewport.style.padding = '10px';
    expect(measureTerminalGrid(viewport, probe)).toEqual({ cols: 100, rows: 23 });
    vi.mocked(viewport.getBoundingClientRect).mockReturnValue({ width: 1606, height: 818 } as DOMRect);
    vi.mocked(probe.getBoundingClientRect).mockReturnValue({ width: 156.5, height: 33.8 } as DOMRect);
    expect(measureTerminalGrid(viewport, probe)).toEqual({ cols: 100, rows: 23 });
  });
  it('rejects hidden, detached and zero-size terminals; recovers on reveal', () => {
    const { viewport, probe } = elements();
    viewport.style.visibility = 'hidden';
    expect(measureTerminalGrid(viewport, probe)).toBeNull();
    viewport.style.visibility = 'visible';
    expect(measureTerminalGrid(viewport, probe)).toEqual({ cols: 102, rows: 24 });
    viewport.remove();
    expect(measureTerminalGrid(viewport, probe)).toBeNull();
  });
  it('observes viewport, probe, visibility, zoom and fonts and tears down every listener', async () => {
    const { viewport, probe } = elements();
    const callbacks: (() => void)[] = [];
    const observed: Element[] = [];
    const disconnect = vi.fn();
    class Observer {
      constructor(cb: () => void) { callbacks.push(cb); }
      observe(element: Element) { observed.push(element); }
      disconnect = disconnect;
    }
    vi.stubGlobal('ResizeObserver', Observer);
    vi.stubGlobal('IntersectionObserver', Observer);
    const fonts = new EventTarget();
    Object.defineProperty(fonts, 'ready', { value: Promise.resolve() });
    Object.defineProperty(document, 'fonts', { configurable: true, value: fonts });
    const zoom = new EventTarget();
    vi.stubGlobal('visualViewport', zoom);
    const fit = vi.fn();
    const stop = observeTerminalGeometry(viewport, probe, fit);
    await Promise.resolve();
    expect(observed).toContain(viewport);
    expect(observed).toContain(probe);
    fit.mockClear();
    for (const cb of callbacks) cb();
    window.dispatchEvent(new Event('resize'));
    window.dispatchEvent(new Event('pageshow'));
    document.dispatchEvent(new Event('visibilitychange'));
    fonts.dispatchEvent(new Event('loadingdone'));
    zoom.dispatchEvent(new Event('resize'));
    viewport.hidden = true;
    await Promise.resolve();
    expect(fit).toHaveBeenCalledTimes(8);
    stop();
    fit.mockClear();
    for (const cb of callbacks) cb();
    window.dispatchEvent(new Event('resize'));
    document.dispatchEvent(new Event('visibilitychange'));
    fonts.dispatchEvent(new Event('loadingdone'));
    zoom.dispatchEvent(new Event('resize'));
    expect(fit).not.toHaveBeenCalled();
    expect(disconnect).toHaveBeenCalledTimes(2);
    Reflect.deleteProperty(document, 'fonts');
  });
});

describe('initial PTY size for a new command (M10)', () => {
  const rect = (width: number, height: number) => ({ width, height, top: 0, left: 0, right: width, bottom: height, x: 0, y: 0, toJSON: () => ({}) }) as DOMRect;
  afterEach(() => { document.body.innerHTML = ''; });
  it('falls back to 100×30 only when nothing is laid out', () => {
    expect(estimatePtySize(document)).toEqual(FALLBACK_PTY_SIZE);
  });
  it('estimates from the stage box and the rendered cell probe', () => {
    document.body.innerHTML = '<main aria-label="Workspace stage"></main><div><span data-terminal-probe></span></div>';
    vi.spyOn(document.querySelector('main')!, 'getBoundingClientRect').mockReturnValue(rect(400, 600));
    vi.spyOn(document.querySelector('span')!, 'getBoundingClientRect').mockReturnValue(rect(80, 20));
    // (400 − 32) / 8 = 46 columns; (600 − 32 − 48) / 20 = 26 rows.
    expect(estimatePtySize(document)).toEqual({ cols: 46, rows: 26 });
  });
});
