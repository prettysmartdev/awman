import type { Env as WorkerEnv } from '../src';
declare global {
  namespace Cloudflare {
    // Cloudflare requires an augmentable interface for test bindings.
    // eslint-disable-next-line @typescript-eslint/no-empty-object-type
    interface Env extends WorkerEnv {}
  }
}
