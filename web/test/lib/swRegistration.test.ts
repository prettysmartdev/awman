// @vitest-environment jsdom
// Review #9: page reload on approved updates, including pages that were open
// (and uncontrolled) during the very first install.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { registerShellWorker, type ShellUpdate } from '@/lib/pwa/serviceWorker';
import { flush } from './helpers';

class FakeWorker extends EventTarget {
  state = 'installing';
  posted: unknown[] = [];
  postMessage(message: unknown) { this.posted.push(message); }
  become(state: string) { this.state = state; this.dispatchEvent(new Event('statechange')); }
}
class FakeRegistration extends EventTarget {
  installing: FakeWorker | null = null;
  waiting: FakeWorker | null = null;
  found(worker: FakeWorker) { this.installing = worker; this.dispatchEvent(new Event('updatefound')); }
}
class FakeContainer extends EventTarget {
  controller: FakeWorker | null = null;
  registration = new FakeRegistration();
  register = vi.fn(async () => this.registration);
  /** The browser switches controller (clients.claim or skipWaiting activation). */
  control(worker: FakeWorker) { this.controller = worker; this.dispatchEvent(new Event('controllerchange')); }
}

function page(container: FakeContainer) {
  vi.stubEnv('NODE_ENV', 'production');
  vi.stubGlobal('navigator', Object.create(navigator, { serviceWorker: { value: container } }));
  const reload = vi.fn();
  const updates: ShellUpdate[] = [];
  const stop = registerShellWorker(update => updates.push(update), reload);
  return { reload, updates, stop };
}

afterEach(() => { vi.unstubAllEnvs(); vi.unstubAllGlobals(); });

describe('registerShellWorker', () => {
  it('first install claims without reloading; a later approved update reloads that same page', async () => {
    const container = new FakeContainer();
    const { reload, updates, stop } = page(container);
    await flush();
    expect(container.register).toHaveBeenCalledWith('/sw.js', { scope: '/', updateViaCache: 'none' });
    const first = new FakeWorker();
    container.registration.found(first);
    first.become('installed'); // no controller yet: first install, no update offer
    expect(updates).toHaveLength(0);
    container.control(first); // activate → clients.claim()
    expect(reload).not.toHaveBeenCalled();

    const second = new FakeWorker();
    container.registration.found(second);
    second.become('installed');
    expect(updates).toHaveLength(1);
    updates[0]!.apply();
    expect(second.posted).toEqual([{ type: 'SKIP_WAITING' }]);
    container.control(second);
    expect(reload).toHaveBeenCalledTimes(1);
    container.control(second);
    expect(reload).toHaveBeenCalledTimes(1);
    stop();
  });

  it('every open controlled page reloads once on replacement', async () => {
    const container = new FakeContainer();
    container.controller = new FakeWorker();
    const a = page(container); const b = page(container);
    await flush();
    container.control(new FakeWorker());
    expect(a.reload).toHaveBeenCalledTimes(1);
    expect(b.reload).toHaveBeenCalledTimes(1);
    a.stop(); b.stop();
  });
});
