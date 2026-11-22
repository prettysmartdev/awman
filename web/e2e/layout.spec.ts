import { expect, test, type Page } from '@playwright/test';
import { createServer } from 'node:http';
import { readFile, rm, mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { root } from './daemon.js';
import { WorkspaceFixture } from './workspace-fixture.js';

test.describe.configure({ timeout: 12_000 });

const originHost = '127.0.0.1';
let origin = '';
let directory = '';
let server: ReturnType<typeof createServer>;
const viewports = [
  { width: 1280, height: 720 }, { width: 1440, height: 900 }, { width: 1024, height: 768 },
  { width: 768, height: 1024 }, { width: 390, height: 844 },
];
const states = ['connect', 'picker', 'workspace', 'concurrent', 'long-output', 'dialog'] as const;
type LayoutState = typeof states[number];

function syntheticWorkspace(state: LayoutState): string {
  const count = state === 'concurrent' ? 2 : 1;
  const terminals = Array.from({ length: count }, (_, index) => `<section class="container-slot ${index === 0 ? 'slot-active' : 'slot-inactive'}" aria-label="Agent ${index + 1}">
    <header><strong>Agent ${index + 1} · Synthetic agent</strong><button type="button" class="secondary">Agent actions</button></header>
    <section class="terminal-panel" aria-label="Terminal: Agent ${index}"><header><span>Terminal · Synthetic agent ${index + 1}</span><button type="button" class="secondary">Terminal actions</button></header>
      <div role="log" aria-label="Agent ${index} output"><span class="terminal-probe" aria-hidden="true" style="display:inline-block;width:10ch;height:1em;font:14px/1 monospace;visibility:hidden">MMMMMMMMMM</span><pre>synthetic-agent$ ready · Synthetic terminal output.</pre></div>
    </section>
  </section>`).join('');
  const output = Array.from({ length: state === 'long-output' ? 180 : 3 }, (_, index) => `<p>Synthetic output line ${String(index + 1).padStart(3, '0')} — no session content or credentials.</p>`).join('');
  const dialog = state === 'dialog' ? `<div class="modal-backdrop"><section class="modal" role="dialog" aria-modal="true" aria-labelledby="synthetic-dialog-title"><header><h2 id="synthetic-dialog-title">Synthetic confirmation</h2></header><div class="dialog-content"><p>Continue with this synthetic action?</p><div class="actions"><button type="button" data-primary>Continue</button><button type="button" class="secondary">Cancel</button></div></div></section></div>` : '';
  return `<div class="shell" data-narrow-panel="agent" data-inspector="none">
    <header class="shell-app-bar glass"><div class="desktop-tabs"><div class="tab-strip"><div role="tablist" aria-label="Workspace tabs"><button role="tab" aria-selected="true">Synthetic workspace</button></div><button type="button" aria-label="New tab">+</button></div></div><button type="button" class="secondary">Actions</button><button type="button" class="secondary">Workspace menu</button></header>
    <div class="shell-context-bar glass"><div class="context-path">/synthetic/workspace</div><button type="button" class="secondary context-chip">Git · synthetic · 2</button><button type="button" class="secondary context-chip">Workflow</button><span class="connection-pill" role="status">Connected · Primary</span></div>
    <div class="shell-agent-chip glass"><button type="button" class="secondary">Active agent · Synthetic agent 1</button></div>
    <div class="shell-body"><main class="shell-stage" aria-label="Workspace stage"><div class="stage-content" data-stage="agent"><div class="stage-heading"><div class="cluster"><button type="button">Agent</button><button type="button" class="secondary">Output</button></div>${count > 1 ? '<label class="agent-selector"><span class="sr-only">Active agent</span><select aria-label="Active agent"><option>Agent 1 · Synthetic</option><option>Agent 2 · Synthetic</option></select></label>' : ''}</div>
      <div class="stage-panes"><section class="shell-containers" aria-label="Agents"><div class="container-area ${count > 1 ? 'synthetic-concurrent' : ''}">${terminals}</div></section><section class="shell-execution" aria-label="Execution area"><h2>Execution</h2><div role="log" aria-label="Synthetic long output">${output}</div></section></div>
    </div></main></div>
    <footer class="shell-composer"><div class="composer-dock"><div class="composer"><label for="synthetic-command">Command</label><div class="composer-input"><textarea id="synthetic-command" aria-label="Command" placeholder="Enter a command"></textarea><button type="button" data-primary>Run</button></div><p class="hint">Enter runs · Shift+Enter inserts a line</p></div></div></footer>
    <nav class="shell-narrow-nav glass" aria-label="Panels"><button type="button" aria-pressed="true">Agent</button><button type="button" aria-pressed="false">Output</button><button type="button">Workflow</button><button type="button">Git</button><button type="button">More</button></nav>${dialog}<div class="shell-notices" aria-live="polite"></div>
  </div>`;
}

function stateMarkup(state: LayoutState): string {
  if (state === 'connect') return `<main class="connect"><section class="connect-card"><h1>Connect browser</h1><p>Connect to the local daemon to open a synthetic workspace.</p><button data-primary>Connect browser</button><button class="secondary">Recheck</button></section></main>`;
  if (state === 'picker') return `<main class="connect"><section class="connect-card"><h1>Choose a workspace</h1><label>Workspace<select aria-label="Workspace"><option>Synthetic workspace · /synthetic/workspace</option></select></label><button data-primary>Open workspace</button><button class="secondary">Attach read only</button></section></main>`;
  return syntheticWorkspace(state);
}

async function startServer() {
  directory = await mkdtemp(join(tmpdir(), 'awman-web-layout-'));
  server = createServer(async (request, response) => {
    response.setHeader('Cache-Control', 'no-store');
    const url = new URL(request.url ?? '/', origin);
    if (url.pathname === '/') {
      response.setHeader('Content-Type', 'text/html');
      const state = states.includes(url.searchParams.get('state') as LayoutState) ? url.searchParams.get('state') as LayoutState : 'workspace';
      response.end(`<!doctype html><html lang="en"><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Synthetic layout check</title><link rel="stylesheet" href="/globals.css"></head><body><div id="root">${stateMarkup(state)}</div></body></html>`);
      return;
    }
    if (url.pathname === '/favicon.ico') { response.writeHead(204).end(); return; }
    const asset = resolve(root, 'web/src/app', url.pathname.slice(1));
    if (!asset.startsWith(resolve(root, 'web/src/app'))) { response.writeHead(403).end(); return; }
    try { response.setHeader('Content-Type', 'text/css'); response.end(await readFile(asset)); }
    catch { response.writeHead(404).end(); }
  });
  await new Promise<void>((resolveListen, reject) => {
    server.once('error', reject);
    server.listen(0, originHost, () => {
      const address = server.address();
      if (!address || typeof address === 'string') { reject(new Error('layout fixture did not bind a TCP port')); return; }
      origin = `http://${originHost}:${address.port}`;
      resolveListen();
    });
  });
}

test.beforeAll(startServer);
test.afterAll(async () => {
  if (server) { server.closeAllConnections(); await new Promise<void>(resolveClose => server.close(() => resolveClose())); }
  if (directory) await rm(directory, { recursive: true, force: true });
});

async function openState(page: Page, state: LayoutState) {
  page.setDefaultNavigationTimeout(7_000);
  await page.goto(`${origin}/?state=${state}`, { waitUntil:'domcontentloaded' });
  if (state === 'connect') await expect(page.getByRole('button', { name: 'Connect browser', exact: true })).toBeVisible();
  if (state === 'picker') await expect(page.getByRole('button', { name: 'Open workspace', exact: true })).toBeVisible();
  if (state === 'dialog') await expect(page.locator('[role="dialog"]')).toBeVisible();
  if (state === 'workspace' || state === 'concurrent' || state === 'long-output') await expect(page.locator('.shell')).toBeVisible({timeout:1000});
}

async function inspectLayout(page: Page, state: LayoutState, width: number) {
  const result = await page.evaluate(() => {
    const rect = (element: Element) => { const box = element.getBoundingClientRect(); return { x:box.x,y:box.y,width:box.width,height:box.height,right:box.right,bottom:box.bottom }; };
    const visible = (element: Element) => { const box=element.getBoundingClientRect(); const style=getComputedStyle(element); return box.width>0 && box.height>0 && style.visibility!=='hidden' && style.display!=='none' && !element.closest('[inert]'); };
    const dialog = document.querySelector('[role="dialog"]');
    const controls = Array.from(document.querySelectorAll('button,input,select,textarea,[role="button"],[role="tab"],a[href]')).filter(el=>visible(el) && !(el as HTMLButtonElement).disabled && (!dialog || el.closest('[role="dialog"]')));
    const overlaps = [];
    for (let i=0;i<controls.length;i++) for (let j=i+1;j<controls.length;j++) {
      const a=controls[i]!, b=controls[j]!; const x=rect(a), y=rect(b);
      const area=Math.max(0,Math.min(x.right,y.right)-Math.max(x.x,y.x))*Math.max(0,Math.min(x.bottom,y.bottom)-Math.max(x.y,y.y));
      if(area>1) overlaps.push({a:a.getAttribute('aria-label')||a.textContent?.trim(),b:b.getAttribute('aria-label')||b.textContent?.trim(),area});
    }
    const primary=Array.from(document.querySelectorAll('button[data-primary]')).find(el=>visible(el));
    const terminal=document.querySelector('.terminal-panel'); const probe=document.querySelector('.terminal-probe');
    const columns=terminal&&probe?Math.floor(terminal.getBoundingClientRect().width/(probe.getBoundingClientRect().width/10)):null;
    const primaryRect=primary?rect(primary):null; const primaryInViewport=!!primaryRect&&primaryRect.x>=0&&primaryRect.y>=0&&primaryRect.right<=innerWidth&&primaryRect.bottom<=innerHeight;
    return {terminalWidth:terminal?.getBoundingClientRect().width,probeWidth:probe?.getBoundingClientRect().width,scrollWidth:document.documentElement.scrollWidth,innerWidth,innerHeight,primaryInViewport,overlaps,controls:controls.map(el=>({name:el.getAttribute('aria-label')||el.textContent?.trim()||el.tagName,rect:rect(el)})),primary:primary?{name:primary.textContent?.trim(),rect:rect(primary)}:null,columns};
  });
  expect(result.scrollWidth, `${state} @ ${width}: page horizontal overflow`).toBeLessThanOrEqual(result.innerWidth);
  expect(result.overlaps, `${state} @ ${width}: overlapping interactive controls`).toEqual([]);
  expect(result.primary, `${state} @ ${width}: visible primary action`).not.toBeNull();
  expect(result.primaryInViewport, `${state} @ ${width}: primary action within viewport`).toBe(true);
  if (width >= 1024 && result.columns !== null) expect(result.columns, `${state} @ ${width}: terminal columns`).toBeGreaterThanOrEqual(60);
  if (width === 390 && await page.evaluate(() => matchMedia('(pointer: coarse)').matches)) expect(result.controls.filter(control=>control.rect.width<44||control.rect.height<44), `${state} @ 390: touch targets below 44px`).toEqual([]);
  return result;
}

for (const state of states) {
  test(`layout ${state}: all five viewports`, async ({ page }) => {
    const results = [];
    await openState(page,state);
    for (const viewport of viewports) {
      await page.setViewportSize(viewport);
      results.push({viewport,state,...await inspectLayout(page,state,viewport.width)});
    }
    expect(results).toHaveLength(viewports.length);
  });
}

test('dark mode, reduced motion, no-backdrop-filter contrast, 200% zoom and keyboard focus', async ({ page }) => {
  await page.emulateMedia({colorScheme:'dark',reducedMotion:'reduce'});
  await page.setViewportSize({width:390,height:844});
  await openState(page,'dialog');
  await page.addStyleTag({content:'.modal-backdrop{backdrop-filter:none!important;-webkit-backdrop-filter:none!important}'});
  const contrast=await page.evaluate(()=>{
    const rgb=(color:string)=>color.match(/[\d.]+/g)?.map(Number).slice(0,3)??[0,0,0];
    const luminance=(values:number[])=>values.map(value=>{const c=value/255;return c<=.04045?c/12.92:((c+.055)/1.055)**2.4;}).reduce((sum,value,index)=>sum+value*[.2126,.7152,.0722][index]!,0);
    const dialog=document.querySelector('.modal')!;const text=dialog.querySelector('h2')!;const a=luminance(rgb(getComputedStyle(dialog).backgroundColor)),b=luminance(rgb(getComputedStyle(text).color));
    return {ratio:(Math.max(a,b)+.05)/(Math.min(a,b)+.05),motion:getComputedStyle(text).transitionDuration,backdrop:getComputedStyle(document.querySelector('.modal-backdrop')!).backdropFilter};
  });
  expect(contrast.ratio).toBeGreaterThanOrEqual(4.5);
  expect(contrast.backdrop).toBe('none');
  expect(contrast.motion, 'reduced-motion preference disables transitions').toBe('0s');
  const names=await page.evaluate(()=>Array.from(document.querySelectorAll('[role="dialog"] button')).filter(el=>{const r=el.getBoundingClientRect();return r.width>0&&r.height>0;}).map(el=>el.textContent?.trim()||''));
  const focused=new Set<string>();
  for(let index=0;index<48;index++){await page.keyboard.press('Tab');focused.add(await page.evaluate(()=>document.activeElement?.textContent?.trim()||''));}
  expect(names.filter(name=>!focused.has(name)),'dialog actions reachable by keyboard without shortcut').toEqual([]);
  await inspectLayout(page,'dialog',390);

  await page.setViewportSize({width:640,height:360});
  await page.evaluate(()=>{document.documentElement.style.zoom='200%';});
  const zoomed=await page.evaluate(()=>({scrollWidth:document.documentElement.scrollWidth,innerWidth}));
  expect(zoomed.scrollWidth).toBeLessThanOrEqual(zoomed.innerWidth);
});

test('workspace controls are reachable by keyboard and pointer without shortcuts', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
  await openState(page, 'workspace');
  const controls = page.locator('button:not(:disabled):visible,input:not(:disabled):visible,select:not(:disabled):visible,textarea:not(:disabled):visible,[role="button"]:visible,[role="tab"]:visible,a[href]:visible');
  const expected = await controls.evaluateAll(elements => elements.filter(element => {
    const rect = element.getBoundingClientRect();
    const style = getComputedStyle(element);
    return rect.width > 0 && rect.height > 0 && style.display !== 'none' && style.visibility !== 'hidden';
  }).map(element => element.getAttribute('aria-label') || element.textContent?.trim() || element.tagName));
  const reached = new Set<string>();
  for (let index = 0; index < expected.length * 3 + 4; index++) {
    await page.keyboard.press('Tab');
    reached.add(await page.evaluate(() => {
      const active = document.activeElement;
      return active?.getAttribute('aria-label') || active?.textContent?.trim() || active?.tagName || '';
    }));
  }
  expect(expected.filter(name => !reached.has(name)), 'all enabled workspace controls reachable without pointer or shortcut').toEqual([]);
  for (let index = 0; index < await controls.count(); index++) await controls.nth(index).click({ trial: true });
});

// ---------------------------------------------------------------------------
// Real production components. The synthetic states above check the stylesheet
// against static markup; the tests below mount the production Workspace,
// Panels, TerminalPanel, TerminalView and Composer (see workspace-fixture.ts)
// so the measured terminal is the one users get.
// ---------------------------------------------------------------------------
test.describe('real workspace', () => {
  test.describe.configure({ timeout: 90_000 });
  const fixture = new WorkspaceFixture();
  test.beforeAll(async () => { await fixture.start(); });
  test.afterAll(async () => { await fixture.stop(); });

  const minRows: Record<string, number> = { '1280x720': 24, '1440x900': 24, '1024x768': 24, '768x1024': 24, '390x844': 12 };

  async function openWorkspace(page: Page, query: string) {
    const violations: string[] = [];
    page.on('console', message => { if (/Content Security Policy/i.test(message.text())) violations.push(message.text()); });
    await page.goto(fixture.url(query));
    await expect(page.getByRole('button', { name: 'Actions', exact: true })).toBeVisible();
    return violations;
  }
  async function showAgent(page: Page) {
    const narrow = await page.evaluate(() => matchMedia('(max-width: 767px)').matches);
    if (narrow) await page.getByRole('navigation', { name: 'Panels' }).getByRole('button', { name: 'Agent', exact: true }).click();
    else if (!await page.locator('section[aria-label^="Terminal:"]:visible').count()) await page.getByRole('group', { name: 'Stage' }).getByRole('button', { name: 'Agent', exact: true }).click();
    await expect(page.locator('section[aria-label^="Terminal:"]:visible')).toHaveCount(1);
    await expect(page.locator('section[aria-label^="Terminal:"]:visible').getByText('Loading terminal…')).toHaveCount(0);
  }
  /** Rows and columns from the real viewport and cell probe (what fit() sends). */
  function grid(page: Page) {
    return page.locator('section[aria-label^="Terminal:"]:visible [role=log]').evaluate(screen => {
      const viewport = screen.parentElement!; const probe = viewport.querySelector('[data-terminal-probe]')!.getBoundingClientRect();
      const box = viewport.getBoundingClientRect();
      return { cols: Math.floor(viewport.clientWidth / (probe.width / 10)), rows: Math.floor(viewport.clientHeight / probe.height), top: box.top, bottom: box.bottom, height: box.height };
    });
  }
  const resizes = (page: Page) => page.evaluate(() => (window as unknown as { __fixture: { resizes: unknown[] } }).__fixture.resizes.length);
  async function settled(page: Page) {
    let last = -1;
    await expect.poll(async () => { const now = await resizes(page); const stable = now === last; last = now; return stable; }, { intervals: [400, 400, 400, 400, 400, 400] }).toBe(true);
    return last;
  }
  async function noResizeAfter(page: Page, before: number, label: string) {
    await page.waitForTimeout(450);
    expect(await resizes(page), `${label}: no PTY resize`).toBe(before);
  }

  test('execution and container resize, minimize and restore without losing the terminal', async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    const violations = await openWorkspace(page, 'slots=1');
    await showAgent(page);
    const execution = page.getByRole('region', { name: 'Execution area' });
    const containers = page.getByRole('region', { name: 'Agents', exact: true });
    const divider = page.getByRole('separator', { name: 'Resize execution and container panels' });
    const dimensions = async () => {
      const e = (await execution.boundingBox())!, c = (await containers.boundingBox())!;
      return { execution: e, containers: c, share: e.width / (e.width + c.width) };
    };
    for (const width of [768, 1440, 1920]) {
      await page.setViewportSize({ width, height: 900 });
      const boxes = await dimensions();
      expect(boxes.execution.x + boxes.execution.width).toBeLessThan(boxes.containers.x);
      expect(boxes.share).toBeCloseTo(1 / 3, 2);
    }
    const before = await grid(page);
    // Wait for the divider to settle after the viewport changes before dragging.
    await divider.hover({ position: { x: 6, y: 100 } });
    const handle = (await divider.boundingBox())!;
    await page.mouse.down();
    await expect(divider).toBeFocused();
    await page.mouse.move(handle.x + 200, handle.y + 100, { steps: 12 });
    await page.mouse.up();
    const resized = await dimensions();
    expect(resized.share).toBeGreaterThan(0.4);
    await expect.poll(async () => (await grid(page)).cols).toBeLessThan(before.cols);
    await settled(page);
    const lastCols = await page.evaluate(() => (window as unknown as { __fixture: { resizes: { cols: number }[] } }).__fixture.resizes.at(-1)!.cols);
    expect(lastCols).toBe((await grid(page)).cols);
    await page.getByRole('button', { name: 'Minimize execution' }).click();
    await expect(execution).toBeHidden();
    await expect(divider).toBeHidden();
    const paneWidth = (await page.locator('.stage-panes').boundingBox())!.width;
    expect((await containers.boundingBox())!.width).toBeCloseTo(paneWidth, 0);
    await page.getByRole('button', { name: 'Minimize container' }).click();
    await expect(containers).toBeHidden();
    await expect(page.getByText('Restore a panel from the bar below.')).toBeVisible();
    await page.getByRole('button', { name: 'Restore Execution', exact: true }).click();
    expect((await execution.boundingBox())!.width).toBeCloseTo(paneWidth, 0);
    await page.getByRole('button', { name: 'Restore Container', exact: true }).click();
    expect((await dimensions()).share).toBeCloseTo(resized.share, 2);
    await divider.focus();
    await page.keyboard.press('ArrowRight');
    expect((await dimensions()).share).toBeCloseTo(resized.share + 0.02, 2);
    await page.keyboard.press('Enter');
    expect((await dimensions()).share).toBeCloseTo(1 / 3, 2);
    await page.setViewportSize({ width: 390, height: 844 });
    await showAgent(page);
    await page.getByRole('button', { name: 'Minimize container' }).click();
    await expect(execution).toBeVisible();
    await page.getByRole('button', { name: 'Restore Container', exact: true }).click();
    await expect(containers).toBeVisible();
    await expect(execution).toBeHidden();
    const connections = await page.evaluate(() => (window as unknown as { __fixture: { terminals: { closed: boolean }[] } }).__fixture.terminals.map(t => t.closed));
    expect(connections).toEqual([false]);
    expect(violations).toEqual([]);
  });

  test('the bottom bar restores the selected minimized agent', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await openWorkspace(page, 'slots=2');
    await showAgent(page);
    await page.getByRole('button', { name: 'Minimize container' }).click();
    await page.getByRole('combobox', { name: 'Active agent' }).selectOption({ index: 1 });
    await page.getByRole('button', { name: 'Minimize container' }).click();
    const bar = page.getByRole('navigation', { name: 'Minimized panels' });
    await expect(bar.getByRole('button')).toHaveCount(2);
    await bar.getByRole('button').first().click();
    await expect(page.getByRole('combobox', { name: 'Active agent' })).toHaveValue('tab_0_slot_0');
    await bar.getByRole('button').click();
    await expect(page.getByRole('combobox', { name: 'Active agent' })).toHaveValue('tab_0_slot_1');
    await expect(bar).toBeHidden();
    await expect(page.locator('section[aria-label^="Terminal:"]:visible')).toHaveCount(1);
  });

  for (const query of ['slots=1', 'slots=3&phase=running']) {
    test(`agent terminal rows, overflow, overlaps and touch targets at five viewports (${query})`, async ({ page }) => {
      const violations = await openWorkspace(page, query);
      for (const viewport of viewports) {
        await page.setViewportSize(viewport);
        await showAgent(page);
        const size = await grid(page);
        const key = `${viewport.width}x${viewport.height}`;
        expect(size.rows, `${key}: agent terminal rows`).toBeGreaterThanOrEqual(minRows[key]!);
        // The default container now has two thirds of the former full-width stage.
        if (viewport.width >= 1024) expect(size.cols, `${key}: split agent terminal columns`).toBeGreaterThanOrEqual(40);
        const layout = await page.evaluate(() => {
          const visible = (el: Element) => { const b = el.getBoundingClientRect(); const s = getComputedStyle(el); return b.width > 0 && b.height > 0 && s.visibility !== 'hidden' && !el.closest('[inert],[hidden]'); };
          const controls = Array.from(document.querySelectorAll('button,select,textarea:not([aria-label^="Type into"]),[role=tab]')).filter(el => visible(el) && !(el as HTMLButtonElement).disabled);
          const boxes = controls.map(el => ({ name: el.getAttribute('aria-label') || el.textContent?.trim() || el.tagName, r: el.getBoundingClientRect() }));
          const overlaps: string[] = [];
          for (let i = 0; i < boxes.length; i++) for (let j = i + 1; j < boxes.length; j++) {
            const a = boxes[i]!.r, b = boxes[j]!.r;
            if (Math.max(0, Math.min(a.right, b.right) - Math.max(a.left, b.left)) * Math.max(0, Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top)) > 1) overlaps.push(`${boxes[i]!.name} × ${boxes[j]!.name}`);
          }
          const small = matchMedia('(pointer: coarse)').matches ? boxes.filter(b => b.r.width < 44 || b.r.height < 44).map(b => `${b.name} ${Math.round(b.r.width)}×${Math.round(b.r.height)}`) : [];
          const header = document.querySelector('section[aria-label^="Terminal:"]:not([hidden] *) > div')!.getBoundingClientRect();
          return { overflow: document.documentElement.scrollWidth - innerWidth, overlaps, small, headerHeight: header.height };
        });
        expect(layout.overflow, `${key}: page horizontal overflow`).toBeLessThanOrEqual(0);
        expect(layout.overlaps, `${key}: overlapping controls`).toEqual([]);
        expect(layout.small, `${key}: touch targets below 44px`).toEqual([]);
        expect(layout.headerHeight, `${key}: one agent header row`).toBeLessThanOrEqual(60);
        await expect(page.locator('[role=toolbar][aria-label="Terminal keys"]:visible')).toHaveCount(await page.evaluate(() => matchMedia('(pointer: coarse)').matches ? 1 : 0));
        if (viewport.width >= 1024) {
          await page.getByRole('button', { name: 'Minimize execution' }).click();
          expect((await grid(page)).cols, `${key}: full-width agent terminal columns`).toBeGreaterThanOrEqual(60);
          await page.getByRole('button', { name: 'Restore Execution', exact: true }).click();
        }
      }
      expect(violations).toEqual([]);
    });
  }

  test('menus, composer focus, suggestions, notices, countdown and terminal errors never resize the PTY', async ({ page }) => {
    for (const viewport of [{ width: 1280, height: 720 }, { width: 390, height: 844 }]) {
      await page.setViewportSize(viewport);
      await openWorkspace(page, 'slots=1');
      await showAgent(page);
      let before = await settled(page);
      const rows = (await grid(page)).rows;
      const terminal = page.locator('section[aria-label^="Terminal:"]:visible');
      await terminal.getByRole('button', { name: 'Agent actions' }).click();
      const menu = page.getByRole('menu', { name: 'Agent actions' });
      await expect(menu).toBeVisible();
      const menuBox = (await menu.boundingBox())!; const gridBox = await grid(page);
      expect(menuBox.y, 'menu overlays the terminal viewport').toBeLessThan(gridBox.bottom);
      await noResizeAfter(page, before, 'agent menu open');
      await page.keyboard.press('Escape');
      await expect(menu).toHaveCount(0);
      await noResizeAfter(page, before, 'agent menu closed');

      await page.getByRole('textbox', { name: 'Command' }).click();
      await page.keyboard.type('re');
      await expect(page.getByLabel('Command suggestions').getByRole('button').first()).toBeVisible();
      await noResizeAfter(page, before, 'composer focus and suggestions');
      expect((await grid(page)).rows).toBe(rows);
      await page.getByRole('textbox', { name: 'Command' }).fill('');
      await page.locator('body').click({ position: { x: 2, y: 2 } });

      await page.evaluate(() => { const f = (window as unknown as { __fixture: { store: { notice(level: string, text: string, source: string): void } } }).__fixture; f.store.notice('info', 'Fixture info notice', 'client'); f.store.notice('error', 'Fixture error notice', 'client'); });
      await expect(page.getByText('Fixture error notice')).toBeVisible();
      const notice = (await page.getByText('Fixture error notice').boundingBox())!;
      const now = await grid(page);
      expect(notice.y, 'notices stay out of the terminal viewport').toBeGreaterThanOrEqual(now.bottom - 1);
      await noResizeAfter(page, before, 'notices');

      await page.evaluate(() => { const f = (window as unknown as { __fixture: { terminals: { closed: boolean; listeners: { error: Set<(e: unknown) => void> } }[] } }).__fixture; for (const t of f.terminals) if (!t.closed) for (const cb of t.listeners.error) cb({ code: 'fixture', message: 'Fixture terminal refusal' }); });
      const alert = terminal.getByRole('alert').filter({ hasText: 'Fixture terminal refusal' });
      await expect(alert).toBeVisible();
      await noResizeAfter(page, before, 'terminal error');
      await alert.getByRole('button', { name: 'Dismiss' }).click();
      await expect(alert).toHaveCount(0);
      await noResizeAfter(page, before, 'terminal error dismissed');
      before = await resizes(page);
      expect((await grid(page)).rows).toBe(rows);
    }
  });

  test('info notices expire on their own; errors stay until dismissed', async ({ page }) => {
    await page.clock.install();
    await openWorkspace(page, 'slots=1');
    const notice = (level: string, text: string) => page.evaluate(([l, t]) => (window as unknown as { __fixture: { store: { notice(level: string, text: string, source: string): void } } }).__fixture.store.notice(l!, t!, 'client'), [level, text]);
    await notice('error', 'Fixture error notice');
    await notice('info', 'Fixture info notice');
    // Newest on top: the info notice shows first, then expires and uncovers the error.
    await expect(page.getByText('Fixture info notice')).toBeVisible();
    await page.clock.runFor(9_000);
    await expect(page.getByText('Fixture info notice')).toHaveCount(0);
    await expect(page.getByText('Fixture error notice')).toBeVisible();
    await page.clock.runFor(30_000);
    await expect(page.getByText('Fixture error notice')).toBeVisible();
    await page.getByRole('alert').filter({ hasText: 'Fixture error notice' }).getByRole('button', { name: 'Dismiss' }).click();
    await expect(page.getByText('Fixture error notice')).toHaveCount(0);
  });

  test('a countdown overlays the agent instead of shortening it', async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    await openWorkspace(page, 'slots=1&countdown=1');
    await showAgent(page);
    await settled(page);
    const banner = page.locator('.embedded-banner');
    await expect(banner).toBeVisible();
    const rows = (await grid(page)).rows;
    expect(rows).toBeGreaterThanOrEqual(24);
  });

  test('focus never jumps into the terminal; Tab passes through without taking control; Ctrl+] leaves', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await openWorkspace(page, 'slots=1&viewer=1');
    await showAgent(page);
    await page.waitForTimeout(300);
    expect(await page.evaluate(() => document.activeElement?.getAttribute('aria-label') ?? document.activeElement?.tagName)).not.toMatch(/^Type into/);
    const input = page.locator('section[aria-label^="Terminal:"]:visible textarea[aria-label^="Type into"]');
    await input.focus();
    await page.keyboard.press('Tab');
    expect(await page.evaluate(() => document.activeElement?.getAttribute('aria-label') ?? '')).not.toMatch(/^Type into/);
    expect(await page.evaluate(() => (window as unknown as { __fixture: { takes?: unknown[]; inputs?: unknown[] } }).__fixture.takes ?? [])).toEqual([]);

    // Sheets restore focus to their opener, never to the terminal.
    const appMenu = page.getByRole('button', { name: 'App menu' });
    await appMenu.click();
    await page.getByRole('menuitem', { name: 'Keyboard shortcuts' }).click();
    const sheet = page.getByRole('dialog', { name: 'Keyboard shortcuts' });
    await expect(sheet).toBeVisible();
    await expect(sheet).toContainText('Ctrl+]');
    await sheet.getByRole('button', { name: 'Close' }).click();
    await expect(appMenu).toBeFocused();

    // As the holder, Tab is the agent's key; Ctrl+] is the documented way out.
    await page.goto(fixture.url('slots=1'));
    await showAgent(page);
    await page.locator('section[aria-label^="Terminal:"]:visible [role=log]').click();
    await expect(input).toBeFocused();
    await page.keyboard.press('Tab');
    await expect(input).toBeFocused();
    expect(await page.evaluate(() => ((window as unknown as { __fixture: { inputs?: number[][] } }).__fixture.inputs ?? []).some(entry => entry.includes(9)))).toBe(true);
    await page.keyboard.press('Control+BracketRight');
    await expect(input).not.toBeFocused();
    expect(await page.evaluate(() => !!document.activeElement?.closest('section[aria-label^="Terminal:"] > div:first-child'))).toBe(true);
  });

  test('focus rings stay whole on tabs, narrow navigation and the agent header', async ({ page }) => {
    const ringInside = (locator: ReturnType<Page['locator']>) => locator.evaluate(element => {
      const style = getComputedStyle(element); const width = parseFloat(style.outlineWidth) || 0; const offset = parseFloat(style.outlineOffset) || 0;
      const box = element.getBoundingClientRect(); const grow = Math.max(0, width + offset);
      const ring = { left: box.left - grow, top: box.top - grow, right: box.right + grow, bottom: box.bottom + grow };
      for (let node = element.parentElement; node; node = node.parentElement) {
        const s = getComputedStyle(node);
        if (s.overflowX === 'visible' && s.overflowY === 'visible') continue;
        const clip = node.getBoundingClientRect();
        if (ring.left < clip.left - 0.5 || ring.top < clip.top - 0.5 || ring.right > clip.right + 0.5 || ring.bottom > clip.bottom + 0.5) return `${element.getAttribute('aria-label') || element.textContent?.trim()} ring clipped by ${node.className || node.tagName}`;
      }
      return style.outlineStyle === 'none' || width === 0 ? `${element.textContent?.trim()} has no focus ring` : '';
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await openWorkspace(page, 'slots=2&tabs=3');
    await showAgent(page);
    await page.keyboard.press('Tab'); // keyboard modality, so :focus-visible applies to focus()
    for (const control of [page.getByRole('tab').first(), page.getByRole('button', { name: /^Close / }).first(), page.getByRole('button', { name: 'New tab' }).first(), page.locator('section[aria-label^="Terminal:"]:visible').getByRole('button', { name: 'Agent actions' }), page.getByRole('combobox', { name: 'Active agent' })]) {
      await control.focus();
      expect(await ringInside(control)).toBe('');
    }
    await page.setViewportSize({ width: 390, height: 844 });
    for (const name of ['Agent', 'Output', 'More']) {
      const button = page.getByRole('navigation', { name: 'Panels' }).getByRole('button', { name, exact: true });
      await button.focus();
      expect(await ringInside(button)).toBe('');
    }
  });

  test('short viewports and 200% zoom keep the agent and the output usable', async ({ page }) => {
    // 640×360, 720×450 and 512×384 are 1280×720, 1440×900 and 1024×768 at 200% zoom; 844×390 is phone landscape.
    await openWorkspace(page, 'slots=1&phase=running');
    for (const viewport of [{ width: 640, height: 360 }, { width: 720, height: 450 }, { width: 512, height: 384 }, { width: 844, height: 390 }]) {
      await page.setViewportSize(viewport);
      await showAgent(page);
      const size = await grid(page);
      expect(size.rows, `${viewport.width}x${viewport.height}: agent rows`).toBeGreaterThanOrEqual(8);
      const composer = page.getByRole('textbox', { name: 'Command' });
      await composer.scrollIntoViewIfNeeded();
      await expect(composer).toBeInViewport();
      const narrow = viewport.width < 768;
      if (narrow) await page.getByRole('navigation', { name: 'Panels' }).getByRole('button', { name: 'Output', exact: true }).click();
      else await page.getByRole('button', { name: 'Minimize container' }).click();
      const output = page.getByRole('region', { name: 'Command output' });
      await output.scrollIntoViewIfNeeded();
      const box = (await output.boundingBox())!;
      expect(box.height, `${viewport.width}x${viewport.height}: output list height`).toBeGreaterThanOrEqual(80);
      await expect(output).toBeInViewport({ ratio: 0.5 });
      await page.evaluate(() => scrollTo(0, 0));
      expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
    }
  });

  test('terminal connections and control survive agent, tab and frontend-socket switches', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await openWorkspace(page, 'slots=2&tabs=2');
    await showAgent(page);
    const opened = () => page.evaluate(() => { const f = (window as unknown as { __fixture: { terminals: { terminalId: string; closed: boolean }[]; closed?: string[] } }).__fixture; return { live: f.terminals.filter(t => !t.closed).map(t => t.terminalId).sort(), closed: f.closed ?? [] }; });
    await expect.poll(async () => (await opened()).live.length).toBe(4);
    const initial = await opened();
    await page.getByRole('combobox', { name: 'Active agent' }).selectOption({ index: 1 });
    await page.getByRole('combobox', { name: 'Active agent' }).selectOption({ index: 0 });
    await page.getByRole('tab').nth(1).click();
    await page.getByRole('tab').nth(0).click();
    await page.evaluate(() => { const c = (window as unknown as { __fixture: { connection: { state: { set(v: string): void } } } }).__fixture.connection; c.state.set('reconnecting'); c.state.set('open'); });
    await page.waitForTimeout(300);
    expect(await opened()).toEqual(initial);
    expect(initial.closed).toEqual([]);
    await expect(page.locator('section[aria-label^="Terminal:"]:visible')).toContainText('You have control');
  });
});
