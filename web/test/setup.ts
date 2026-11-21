// Node-only protocol tests need a page hostname before their modules import.
// jsdom component tests supply their own location.
if (typeof globalThis.location === 'undefined') {
  Object.defineProperty(globalThis, 'location', {
    value: { hostname: '127.0.0.1', protocol: 'http:' },
    configurable: true,
  });
}
