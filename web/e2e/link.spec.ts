// Contract-driven browser link tests. Missing WI 0159 browser support is a
// failure, never mistaken for an unavailable optional WI 0143 capability.
import type { Page } from '@playwright/test';
import { test, expect } from './fixtures.js';
import { endpoint, origin, type Daemon } from './daemon.js';

/**
 * The bearer from the app's own approved poll, kept only in test memory (never
 * logged) so the raw competing-socket case can obtain a ticket like a client
 * that bypasses the app.
 */
function captureBearer(page: Page) {
  let bearer: { id: string; token: string } | null = null;
  page.on('response', response => {
    if (!/\/v1\/browser-links\/[^/]+\/poll$/.test(response.url()) || response.status() !== 200) return;
    void response.json().then(body => {
      if (body?.status === 'approved' && typeof body.grant?.token === 'string') bearer = { id: body.grant.id, token: body.grant.token };
    }, () => undefined);
  });
  return () => bearer;
}

async function request(page: Page, path: string, body?: unknown, token?: string) {
  return page.evaluate(async ({ endpoint, path, body, token }) => {
    const response = await fetch(endpoint + path, {
      method: body === undefined ? 'GET' : 'POST', cache: 'no-store',
      headers: { ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
        ...(token ? { Authorization: `Bearer ${token}` } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    return { status: response.status, cache: response.headers.get('Cache-Control'), body: await response.json() };
  }, { endpoint, path, body, token });
}

async function create(page: Page) {
  await page.evaluate(endpoint => {
    document.querySelector('button')!.addEventListener('click', () => {
      // Drain the fetch body so Chromium emits loadingFinished for the
      // Playwright response reader even when the fixture discards the value.
      void fetch(endpoint + '/v1/browser-links', {
        method: 'POST', cache: 'no-store', headers: { 'Content-Type': 'application/json' }, body: '{}',
      }).then(response => response.json());
    }, { once: true });
  }, endpoint);
  const [response] = await Promise.all([
    page.waitForResponse(endpoint + '/v1/browser-links'), page.getByRole('button', { name: 'Probe' }).click(),
  ]);
  expect(response.status()).toBe(201);
  expect(response.headers()['cache-control']).toContain('no-store');
  return await response.json() as { id: string; code: string; polling_secret: string; interval_ms: number; expires_at: string };
}

test.beforeEach(async ({ page, daemon }) => {
  await daemon.allowOrigin();
  await daemon.recipe();
  await daemon.start();
  await page.goto('/__transport_fixture');
  const health = await request(page, '/v1/health');
  expect(health.status).toBe(200);
  expect(health.body.browser_links, 'WI 0159 browser-link implementation is required').toBe(true);
  expect(health.body.protocols.browser).toBe(1);
});

test('explicit owner confirmation, launch context, scope and revocation', async ({ page, daemon }) => {
  const link = await create(page);
  const output = await daemon.confirmLink(link.code, true);
  expect(output).toContain(origin);
  expect(output).toContain('loopback');
  expect(output).toContain('engine');
  await page.waitForTimeout(link.interval_ms);
  const poll = await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret });
  expect(poll.body.status).toBe('approved');
  expect(poll.cache).toContain('no-store');
  const grant = poll.body.grant;
  expect(grant.scope).toBe('engine');
  expect(grant.origin).toBe(origin);
  expect(grant.listener).toBe('loopback');
  expect((await request(page, '/v1/engine/launch_contexts', undefined, grant.token)).status).toBe(200);
  expect((await request(page, '/v1/worker/workdirs', undefined, grant.token)).status).toBe(404);
  await page.waitForTimeout(link.interval_ms);
  expect((await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret })).status).toBe(410);
  await daemon.cli('daemon', 'api', 'browsers.delete', `id=${grant.id}`);
  expect((await request(page, '/v1/status', undefined, grant.token)).status).toBe(401);
});

test('owner denial consumes the link without a grant', async ({ page, daemon }) => {
  const link = await create(page);
  await daemon.confirmLink(link.code, false);
  await page.waitForTimeout(link.interval_ms);
  expect((await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret })).body.status).toBe('denied');
  await page.waitForTimeout(link.interval_ms);
  expect((await request(page, `/v1/browser-links/${link.id}/poll`, { polling_secret: link.polling_secret })).status).toBe(410);
});

// ---------------------------------------------------------------------------
// WI 0160: reload-safe reauthorization through the real app, real browser
// refreshes and the real daemon. Scope notes (SR-10), so no test claims more
// than it exercises:
// - the 403 "expired verdict" case rewrites one daemon answer (a twelve-hour
//   clock cannot pass here). It tests the client's rendering and cleanup of
//   that verdict, NOT daemon expiry enforcement, which is the Rust component
//   tier's job;
// - the cloned-hint case shows the cooperative Web Lock stops a second
//   document before Hello. Daemon-side primary enforcement against a client
//   that skips the lock is the separate raw competing-socket case below;
// - the storage audit runs with service workers blocked (config default); the
//   installed/controlling worker case below inspects CacheStorage keys and
//   response bodies with a real worker in control.

const COPY = {
  restored: 'Reconnected to your workspace. Commands and answers were not resent. Check the current status before repeating an action.',
  expired: "This browser's 12-hour authorization has expired. Connect this browser again and approve the new link command.",
  revoked: "This browser's access was revoked. Connect this browser again only if the owner wants to restore access.",
  restarted: 'The daemon has restarted since this browser was linked. Its previous authorization is no longer valid. Connect this browser again.',
  stopped: 'The daemon is stopping or has stopped. Start it on this machine, then choose Recheck. You will need to link this browser again after it restarts.',
  otherPrimary: 'Another browser tab or frontend holds primary control of this workspace. You can view it here, return to the primary tab, or open a new workspace.',
  originMismatch: 'This authorization belongs to a different site origin. Return to the original site or connect this browser here with a new approval.',
};
const RESUME_KEY = 'awman.web.resume';
type Frame = { type?: string; [key: string]: unknown };

/** Every frontend frame sent/received, every link creation and every reauth request. */
function observe(page: Page) {
  const sent: Frame[] = [];
  const received: Frame[] = [];
  const sockets: string[] = [];
  const requests: string[] = [];
  page.on('websocket', socket => {
    sockets.push(socket.url());
    socket.on('framesent', event => { try { sent.push(JSON.parse(String(event.payload))); } catch { /* binary */ } });
    socket.on('framereceived', event => { try { received.push(JSON.parse(String(event.payload))); } catch { /* binary */ } });
  });
  page.on('request', request => {
    if (request.url().startsWith(endpoint) && request.method() === 'POST') requests.push(new URL(request.url()).pathname);
  });
  // A running command arms the leave warning; accept it as a user would.
  page.on('dialog', dialog => void dialog.accept());
  return { sent, received, sockets, requests, reset() { sent.length = 0; received.length = 0; sockets.length = 0; requests.length = 0; } };
}

async function allowLocal(page: Page) {
  const permission = page.getByRole('button', { name: 'Allow local access', exact: true });
  if (await permission.isVisible().catch(() => false)) await permission.click();
}

/** Explicit Connect browser + owner approval in a real PTY; ends at the workspace picker. */
async function linkApp(page: Page, daemon: Daemon) {
  await page.goto('/');
  await allowLocal(page);
  await page.getByRole('button', { name: 'Connect browser', exact: true }).click();
  const text = await page.getByText(/awman link web-[A-Z0-9]{4}-[A-Z0-9]{4}/).first().textContent();
  const output = await daemon.confirmLink(text!.match(/web-[A-Z0-9]{4}-[A-Z0-9]{4}/)![0], true);
  expect(output).toContain('This browser may reconnect without another approval until this authorization expires, for up to 12 hours.');
  await expect(page.getByRole('heading', { name: 'Choose a workspace' })).toBeVisible();
}

/** A new workspace runs `ready`, which waits on a real "Create Dockerfile?" dialog (no Docker needed). */
async function openWorkspace(page: Page) {
  await page.getByRole('button', { name: 'Open workspace', exact: true }).click();
  await expect(page.getByRole('dialog', { name: 'Create Dockerfile?' })).toBeVisible({ timeout: 20_000 });
}

async function resumeHint(page: Page) {
  return page.evaluate(key => JSON.parse(sessionStorage.getItem(key) ?? 'null'), RESUME_KEY) as Promise<Record<string, unknown> | null>;
}

/** Public metadata of the saved credentials (never key material). */
async function savedCredentials(page: Page) {
  return page.evaluate(() => new Promise<Record<string, unknown>[]>((resolve, reject) => {
    const open = indexedDB.open('awman-browser-auth', 1);
    open.onerror = () => reject(open.error);
    open.onsuccess = () => {
      const db = open.result;
      if (!db.objectStoreNames.contains('credentials')) { db.close(); resolve([]); return; }
      const all = db.transaction('credentials').objectStore('credentials').getAll();
      all.onsuccess = () => {
        db.close();
        resolve((all.result as Record<string, unknown>[]).map(({ privateKey, ...rest }) => {
          const key = privateKey as CryptoKey;
          return { ...rest, key: { type: key.type, extractable: key.extractable, usages: key.usages, algorithm: (key.algorithm as EcKeyAlgorithm).namedCurve } };
        }));
      };
      all.onerror = () => reject(all.error);
    };
  }));
}

async function linkedWorkspace(page: Page, daemon: Daemon) {
  await daemon.launchContext();
  await linkApp(page, daemon);
  await openWorkspace(page);
  await expect.poll(() => resumeHint(page)).toMatchObject({ version: 2 });
  return (await resumeHint(page))!;
}

test.describe('reload-safe reauthorization (WI 0160)', () => {
  test('valid reload with a running command and pending dialog: no relink, same instance and tab, nothing resent', async ({ page, daemon }) => {
    const seen = observe(page);
    const hint = await linkedWorkspace(page, daemon);
    const [grant] = await daemon.browsers();
    expect(grant!.reauth?.credential_id).toBe(hint.credentialId);
    const creates = seen.requests.filter(path => path === '/v1/browser-links').length;
    expect(creates).toBe(1);
    expect(seen.sent.filter(frame => frame.type === 'submit')).toEqual([]);
    expect(seen.received.some(frame => frame.type === 'delta' && (frame.value as { phase?: { phase?: string } })?.phase?.phase === 'running')).toBe(true);
    const welcome = seen.received.find(frame => frame.type === 'welcome')!;
    seen.reset();

    await page.reload();
    await expect(page.getByRole('dialog', { name: 'Create Dockerfile?' })).toBeVisible({ timeout: 20_000 });
    await expect(page.getByText(COPY.restored)).toBeVisible();
    // No link was created, approved or offered; the saved key proved the same grant.
    expect(seen.requests).not.toContain('/v1/browser-links');
    expect(seen.requests).toEqual(expect.arrayContaining(['/v1/browser-reauth/challenge', '/v1/browser-reauth/complete']));
    await expect(page.getByRole('button', { name: 'Connect browser', exact: true })).toHaveCount(0);
    expect((await daemon.browsers()).map(b => b.id)).toEqual([grant!.id]);
    // Same instance, same frontend (resumed, not a new primary) and same tab.
    const frontends = seen.sockets.filter(url => url.endsWith('/frontend'));
    expect(frontends.length).toBeGreaterThan(0);
    expect(frontends.every(url => url.endsWith(`/v1/engine/instances/${hint.instanceId}/frontend`))).toBe(true);
    const hello = seen.sent.find(frame => frame.type === 'hello')!;
    expect(hello.resume).toBe(hint.frontendId);
    expect(hello).not.toHaveProperty('launch_context');
    expect(seen.received.find(frame => frame.type === 'welcome')!.frontend_id).toBe(welcome.frontend_id);
    expect(await resumeHint(page)).toMatchObject({ instanceId: hint.instanceId, frontendId: hint.frontendId, tabId: hint.tabId });
    // The command is still the same running one; nothing was resubmitted or answered.
    const snapshot = seen.received.find(frame => frame.type === 'snapshot') as { tabs: { tab_id: string; execution_phase: { phase: string } }[] };
    expect(snapshot.tabs.find(tab => tab.tab_id === hint.tabId)!.execution_phase.phase).toBe('running');
    expect(seen.sent.filter(frame => frame.type === 'submit' || frame.type === 'dialog_response')).toEqual([]);

    // Answer once after the reload: exactly one answer reaches the engine and the dialog closes.
    await page.getByRole('dialog', { name: 'Create Dockerfile?' }).getByRole('button', { name: 'No', exact: true }).click();
    await expect(page.getByRole('dialog', { name: 'Create Dockerfile?' })).toHaveCount(0);
    await page.reload();
    await expect(page.getByText(COPY.restored)).toBeVisible({ timeout: 20_000 });
    await page.waitForTimeout(1000);
    expect(seen.sent.filter(frame => frame.type === 'dialog_response')).toHaveLength(1);
    expect(seen.sent.filter(frame => frame.type === 'submit')).toEqual([]);
    await expect(page.getByRole('dialog', { name: 'Create Dockerfile?' })).toHaveCount(0);
    expect(seen.requests.filter(path => path === '/v1/browser-links')).toEqual([]);
  });

  test('UI rendering of a 403 authorization_expired proof verdict: key discarded, new link offered (rewritten response; not daemon expiry enforcement)', async ({ page, daemon }) => {
    await linkedWorkspace(page, daemon);
    // Real challenge and proof; only the daemon's verdict is rewritten to the
    // expired answer. A broken daemon expiry check would still pass this test:
    // it covers client presentation/cleanup only.
    await page.route(`${endpoint}/v1/browser-reauth/complete`, async route => {
      const response = await route.fetch();
      await route.fulfill({ response, status: 403, body: JSON.stringify({ code: 'authorization_expired', error: 'Browser authorization expired.' }) });
    });
    await page.reload();
    await expect(page.getByText(COPY.expired)).toBeVisible({ timeout: 20_000 });
    await expect(page.getByRole('button', { name: 'Connect browser', exact: true })).toBeVisible();
    await expect.poll(() => savedCredentials(page)).toEqual([]);
    expect(await resumeHint(page)).toBeNull();
  });

  test('revocation: live sockets end with the revoked state; a reload cannot recover', async ({ page, daemon }) => {
    const seen = observe(page);
    await linkedWorkspace(page, daemon);
    const [grant] = await daemon.browsers();
    await daemon.revoke(grant!.id);
    await expect(page.getByText(COPY.revoked)).toBeVisible({ timeout: 20_000 });
    await expect.poll(() => savedCredentials(page)).toEqual([]);
    seen.reset();
    await page.reload();
    await allowLocal(page);
    await expect(page.getByRole('button', { name: 'Connect browser', exact: true })).toBeVisible();
    expect(seen.requests).toEqual([]);
  });

  test('revocation while the page is closed: the reload proof reports revoked', async ({ page, daemon }) => {
    await linkedWorkspace(page, daemon);
    const [grant] = await daemon.browsers();
    await page.goto('/__transport_fixture');
    await daemon.revoke(grant!.id);
    await page.goto('/');
    await allowLocal(page);
    await expect(page.getByText(COPY.revoked)).toBeVisible({ timeout: 20_000 });
    await expect.poll(() => savedCredentials(page)).toEqual([]);
  });

  test('daemon restart: stop is reported, then the new generation needs a new link (never a stale resume)', async ({ page, daemon }) => {
    await linkedWorkspace(page, daemon);
    await daemon.stop();
    await expect(page.getByText(COPY.stopped)).toBeVisible({ timeout: 20_000 });
    await daemon.start();
    await page.reload();
    await allowLocal(page);
    await expect(page.getByText(COPY.restarted)).toBeVisible({ timeout: 20_000 });
    await expect(page.getByRole('button', { name: 'Connect browser', exact: true })).toBeVisible();
    await expect.poll(() => savedCredentials(page)).toEqual([]);
    expect(await resumeHint(page)).toBeNull();
  });

  test('multiple tabs recover one grant independently; the cooperative Web Lock stops a cloned hint before Hello', async ({ page, daemon, context }) => {
    const hint = await linkedWorkspace(page, daemon);
    const [grant] = await daemon.browsers();
    // An independent tab without a hint recovers the same grant and opens the picker, not the instance.
    const second = await context.newPage();
    const secondSeen = observe(second);
    await second.goto('/');
    await allowLocal(second);
    await expect(second.getByRole('heading', { name: 'Choose a workspace' })).toBeVisible({ timeout: 20_000 });
    expect(secondSeen.requests).not.toContain('/v1/browser-links');
    expect(secondSeen.sockets).toEqual([]);
    expect((await daemon.browsers()).map(b => b.id)).toEqual([grant!.id]);
    // The first tab's bearer still works: its workspace stays live and writable.
    await page.reload();
    await expect(page.getByText(COPY.restored)).toBeVisible({ timeout: 20_000 });
    // A duplicated tab (same sessionStorage hint) while the primary is connected.
    const third = await context.newPage();
    await third.addInitScript(([key, value]) => { if (!sessionStorage.getItem(key)) sessionStorage.setItem(key, value); }, [RESUME_KEY, JSON.stringify(hint)] as const);
    const thirdSeen = observe(third);
    await third.goto('/');
    await allowLocal(third);
    await expect(third.getByText(COPY.otherPrimary)).toBeVisible({ timeout: 20_000 });
    // Zero Hello frames: the Web Lock (client cooperation) stopped it. This says
    // nothing about daemon enforcement; see the raw competing-socket case.
    expect(thirdSeen.sent.filter(frame => frame.type === 'hello')).toEqual([]);
    await expect(page.getByRole('dialog', { name: 'Create Dockerfile?' })).toBeVisible();
    await expect(page.getByText(COPY.otherPrimary)).toHaveCount(0);
  });

  test('origin mismatch: a record bound to another origin is never proven', async ({ page, daemon }) => {
    const seen = observe(page);
    await linkedWorkspace(page, daemon);
    await page.evaluate(() => new Promise<void>((resolve, reject) => {
      const open = indexedDB.open('awman-browser-auth', 1);
      open.onsuccess = () => {
        const store = open.result.transaction('credentials', 'readwrite').objectStore('credentials');
        const all = store.getAll();
        all.onsuccess = () => {
          for (const record of all.result) store.put({ ...record, origin: 'http://localhost:3000' });
          store.transaction.oncomplete = () => { open.result.close(); resolve(); };
        };
      };
      open.onerror = () => reject(open.error);
    }));
    seen.reset();
    await page.reload();
    await allowLocal(page);
    await expect(page.getByText(COPY.originMismatch)).toBeVisible({ timeout: 20_000 });
    expect(seen.requests).toEqual([]);
    await expect(page.getByRole('button', { name: 'Connect browser', exact: true })).toBeVisible();
  });

  test('browser storage holds only the declared key record and resume ids (service workers blocked)', async ({ page, daemon }) => {
    const seen = observe(page);
    await linkedWorkspace(page, daemon);
    seen.reset();
    await page.reload();
    await expect(page.getByText(COPY.restored)).toBeVisible({ timeout: 20_000 });
    const dump = await page.evaluate(async () => {
      const local = Object.fromEntries(Object.entries(localStorage));
      const session = Object.fromEntries(Object.entries(sessionStorage));
      const names = typeof indexedDB.databases === 'function' ? (await indexedDB.databases()).map(db => db.name) : ['awman-browser-auth'];
      const databases: Record<string, Record<string, unknown[]>> = {};
      for (const name of names) {
        if (!name) continue;
        const db = await new Promise<IDBDatabase>((resolve, reject) => { const open = indexedDB.open(name); open.onsuccess = () => resolve(open.result); open.onerror = () => reject(open.error); });
        databases[name] = {};
        for (const store of Array.from(db.objectStoreNames)) {
          const rows = await new Promise<unknown[]>(resolve => { const all = db.transaction(store).objectStore(store).getAll(); all.onsuccess = () => resolve(all.result); });
          databases[name][store] = await Promise.all(rows.map(async row => {
            const out: Record<string, unknown> = {};
            for (const [key, value] of Object.entries(row as object)) {
              if (value instanceof CryptoKey) {
                const exported = await crypto.subtle.exportKey('pkcs8', value).then(() => 'EXPORTED', () => 'refused');
                out[key] = { CryptoKey: { type: value.type, extractable: value.extractable, usages: value.usages, exported } };
              } else out[key] = value;
            }
            return out;
          }));
        }
        db.close();
      }
      const cacheNames = await caches.keys();
      const cached: string[] = [];
      for (const name of cacheNames) for (const request of await (await caches.open(name)).keys()) cached.push(request.url);
      return { local, session, databases, cacheNames, cached, cookie: document.cookie };
    });
    expect(dump.local).toEqual({});
    expect(dump.cookie).toBe('');
    expect(dump.cacheNames.filter(name => !name.startsWith('awman-shell-'))).toEqual([]);
    expect(dump.cached.filter(url => new URL(url).origin !== origin || url.includes('/v1/'))).toEqual([]);
    expect(Object.keys(dump.session).every(key => key === 'awman.web.resume' || key === 'awman.web.view')).toBe(true);
    expect(Object.keys(JSON.parse(dump.session['awman.web.resume']!)).sort()).toEqual(['credentialId', 'daemonEpoch', 'frontendId', 'instanceId', 'launchContext', 'listenerEpoch', 'tabId', 'version']);
    expect(Object.keys(dump.databases)).toEqual(['awman-browser-auth']);
    const rows = dump.databases['awman-browser-auth']!.credentials as Record<string, unknown>[];
    expect(rows).toHaveLength(1);
    expect(Object.keys(rows[0]!).sort()).toEqual(['credentialId', 'daemonBase', 'daemonEpoch', 'expiresAt', 'grantId', 'linkExpiresAt', 'listener', 'listenerEpoch', 'origin', 'privateKey', 'publicKey', 'version']);
    expect(rows[0]!.privateKey).toEqual({ CryptoKey: { type: 'private', extractable: false, usages: ['sign'], exported: 'refused' } });
    // No bearer, ticket, polling secret, proof, command, dialog or path anywhere in storage.
    const text = JSON.stringify(dump);
    for (const forbidden of [/awb_/, /awt_/, /polling/i, /signature/i, /Dockerfile/, /\bready\b/, /\/tmp\/|\/home\//, /snapshot/i, /"token"/]) expect(text).not.toMatch(forbidden);
    expect(seen.requests).not.toContain('/v1/browser-links');
  });
});

// Daemon-side primary enforcement (SR-10): a client that ignores the Web Lock.
// A second same-origin document with no app code fetches a fresh ticket with
// the live grant and sends Hello itself. Positive control: the same harness is
// admitted as a secondary, so the primary refusals are the daemon's decision,
// not a malformed raw client.
test('raw competing frontend socket bypassing Web Locks: the daemon refuses a second primary', async ({ page, daemon, context }) => {
  const bearer = captureBearer(page);
  const seen = observe(page);
  const hint = await linkedWorkspace(page, daemon);
  await expect.poll(() => bearer() !== null).toBe(true);
  const primaryHello = seen.sent.find(frame => frame.type === 'hello')!;
  expect(primaryHello).toBeTruthy();
  const rogue = await context.newPage();
  await rogue.goto('/__transport_fixture');
  const attempt = (hello: Frame) => rogue.evaluate(async ({ endpoint, token, instanceId, hello }) => {
    const ticketResponse = await fetch(endpoint + '/v1/browser-tickets', {
      method: 'POST', cache: 'no-store', credentials: 'omit',
      headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
      body: JSON.stringify({ route: 'frontend', instance_id: instanceId }),
    });
    if (!ticketResponse.ok) return { ticketStatus: ticketResponse.status, frames: [] as Record<string, unknown>[], code: 0, protocol: '' };
    const { ticket } = await ticketResponse.json() as { ticket: string };
    const ws = endpoint.replace(/^http:/, 'ws:');
    return await new Promise<{ ticketStatus: number; frames: Record<string, unknown>[]; code: number; protocol: string }>(resolve => {
      const frames: Record<string, unknown>[] = [];
      const socket = new WebSocket(`${ws}/v1/engine/instances/${encodeURIComponent(instanceId)}/frontend`, ['awman.frontend.v1', `awman.ticket.${ticket}`]);
      const done = (code: number) => { clearTimeout(timer); resolve({ ticketStatus: ticketResponse.status, frames, code, protocol: socket.protocol }); };
      const timer = setTimeout(() => { socket.close(1000); done(-1); }, 8000);
      socket.onopen = () => socket.send(JSON.stringify(hello));
      socket.onmessage = event => {
        try { frames.push(JSON.parse(String(event.data))); } catch { frames.push({ unparsable: true }); }
        if (frames.some(frame => frame.type === 'welcome')) { socket.close(1000); }
      };
      socket.onclose = event => done(event.code);
    });
  }, { endpoint, token: bearer()!.token, instanceId: hint.instanceId as string, hello });
  // The app's first Hello (fresh primary with its launch context), minus any resume.
  const { resume: _resume, ...fresh } = primaryHello;
  const welcomed = (result: { frames: Record<string, unknown>[] }) => result.frames.some(frame => frame.type === 'welcome');

  // Positive control: the raw harness can attach (as a secondary).
  const { launch_context: _context, ...secondaryBase } = fresh;
  const secondary = await attempt({ ...secondaryBase, seq: 1, type: 'hello', role: 'secondary', view: 'all' });
  expect(secondary.ticketStatus).toBeLessThan(300);
  expect(secondary.protocol).toBe('awman.frontend.v1');
  expect(welcomed(secondary)).toBe(true);

  // A fresh primary Hello and a stolen-resume primary Hello are both refused by the daemon.
  for (const hello of [{ ...fresh, seq: 1, type: 'hello', role: 'primary' }, { ...secondaryBase, seq: 1, type: 'hello', role: 'primary', resume: hint.frontendId }] as Frame[]) {
    const result = await attempt(hello);
    expect(result.ticketStatus).toBeLessThan(300);
    expect(welcomed(result), `daemon admitted a competing primary (${hello.resume ? 'resume' : 'fresh'})`).toBe(false);
    expect(result.frames.filter(frame => !('seq' in frame)).map(frame => frame.code), JSON.stringify(result.frames)).toEqual(['primary_exists']);
    expect(result.code).not.toBe(-1);
  }
  // The real primary is untouched: still connected, same dialog, no other-primary notice.
  await expect(page.getByRole('dialog', { name: 'Create Dockerfile?' })).toBeVisible();
  await expect(page.getByText(COPY.otherPrimary)).toHaveCount(0);
  expect(seen.sent.filter(frame => frame.type === 'dialog_response' || frame.type === 'submit')).toEqual([]);
});

// Installed and controlling service worker (SR-10). Every other test blocks
// workers (playwright.config.ts). Here the production out/sw.js installs, takes
// control and stays in control across real daemon traffic and reloads; then
// every CacheStorage entry's key AND response body is inspected for daemon or
// credential data. Negative control (documented in the review resolution): a
// copy of the export whose worker writes daemon GET responses into the cache,
// served via AWMAN_E2E_WEB_OUT, makes this test fail.
test.describe('service worker in control', () => {
  test.use({ serviceWorkers: 'allow' });

  test('CacheStorage keys and response bodies hold only shell assets, never daemon or credential data', async ({ page, daemon }) => {
    const bearer = captureBearer(page);
    const seen = observe(page);
    const hint = await linkedWorkspace(page, daemon);
    expect(await page.evaluate(async () => (await navigator.serviceWorker.ready).active?.scriptURL ?? null)).toBe(`${origin}/sw.js`);
    await expect.poll(() => page.evaluate(() => navigator.serviceWorker.controller?.scriptURL ?? null)).toBe(`${origin}/sw.js`);
    // Daemon traffic while controlled: reload proof (challenge/complete), tickets, GETs and sockets.
    seen.reset();
    await page.reload();
    await expect(page.getByText(COPY.restored)).toBeVisible({ timeout: 20_000 });
    expect(await page.evaluate(() => navigator.serviceWorker.controller?.scriptURL ?? null)).toBe(`${origin}/sw.js`);
    expect(seen.requests).toEqual(expect.arrayContaining(['/v1/browser-reauth/challenge', '/v1/browser-reauth/complete']));
    await page.reload();
    await expect(page.getByRole('dialog', { name: 'Create Dockerfile?' })).toBeVisible({ timeout: 20_000 });
    await expect.poll(() => bearer() !== null).toBe(true);
    const [grant] = await daemon.browsers();
    const secrets = [bearer()!.token, grant!.id, hint.credentialId, hint.instanceId, hint.frontendId, hint.tabId, hint.daemonEpoch, hint.listenerEpoch]
      .filter((value): value is string => typeof value === 'string' && value.length >= 8);
    expect(secrets.length).toBeGreaterThanOrEqual(6);
    const report = await page.evaluate(async ({ secrets, patterns }) => {
      const regexes = patterns.map(source => new RegExp(source, 'i'));
      const entries: { cache: string; url: string; method: string; status: number; type: string; bytes: number; hits: string[] }[] = [];
      for (const name of await caches.keys()) {
        const cache = await caches.open(name);
        for (const request of await cache.keys()) {
          const response = await cache.match(request);
          const text = response ? await response.clone().text() : '';
          const headers = response ? JSON.stringify(Array.from(response.headers as unknown as Iterable<[string, string]>)) : '';
          const hits = [
            ...secrets.map((secret, i) => (text.includes(secret) || headers.includes(secret) || request.url.includes(secret) ? `secret#${i}` : null)).filter((hit): hit is string => hit !== null),
            ...regexes.filter(re => re.test(text) || re.test(headers)).map(re => re.source),
          ];
          entries.push({ cache: name, url: request.url, method: request.method, status: response?.status ?? 0, type: response?.type ?? '', bytes: text.length, hits });
        }
      }
      return entries;
    }, {
      secrets,
      // Shapes of daemon/session data: bearer/ticket tokens, resource ids, reauth fields, home paths, daemon JSON keys.
      patterns: ['awb_[A-Za-z0-9_-]{16}', 'awt_[A-Za-z0-9_-]{16}', '\\b(?:brw|ins|lc|term|tab|frn)_[0-9a-f]{32}\\b', 'polling_secret"\\s*:', '"challenge_id"\\s*:', '"signature"\\s*:', '"grant"\\s*:\\s*\\{', '"instance_id"\\s*:', '"frontend_id"\\s*:', '/tmp/aw159-', '(?:/Users/|/home/)[A-Za-z0-9._-]+/'],
    });
    // The worker really cached the shell (positive control for the inspection itself).
    expect(report.length).toBeGreaterThan(3);
    expect(report.some(entry => new URL(entry.url).pathname === '/')).toBe(true);
    expect(report.reduce((sum, entry) => sum + entry.bytes, 0)).toBeGreaterThan(1000);
    // Only versioned shell caches, same-origin GET keys, no API paths or queries, no daemon/credential bytes.
    expect([...new Set(report.map(entry => entry.cache))].every(name => name.startsWith('awman-shell-'))).toBe(true);
    const bad = report.filter(entry => {
      const url = new URL(entry.url);
      return url.origin !== origin || entry.method !== 'GET' || url.search !== '' || /^\/(?:v\d+|api|__owner)(?:\/|$)/.test(url.pathname) || entry.hits.length > 0 || entry.type === 'cors' || entry.type === 'opaque';
    }).map(entry => ({ url: entry.url, hits: entry.hits, type: entry.type }));
    expect(bad).toEqual([]);
  });
});
