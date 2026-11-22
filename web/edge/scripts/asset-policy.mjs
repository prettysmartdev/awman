import { parse } from 'parse5';

// Parse HTML as a browser does, rather than searching strings. This checks the
// generated shell, not user/terminal text (which must never reach this build).
export function auditHtml(html) {
  const problems = new Set();
  const localReferences = new Set();
  const visit = node => {
    const name = node.tagName;
    const attrs = new Map((node.attrs ?? []).map(attr => [attr.name, attr.value]));
    const script = name === 'script';
    const linkedResource = name === 'link' && /(?:stylesheet|preload|modulepreload|icon|manifest)/.test(attrs.get('rel') ?? '');
    if (script && (!attrs.get('src') || node.childNodes?.some(child => child.value?.trim()))) problems.add('inline-script');
    if (name === 'style' || attrs.has('style')) problems.add('inline-style');
    for (const attr of attrs.keys()) if (/^on/i.test(attr)) problems.add('inline-handler');
    if (['iframe', 'frame', 'object', 'embed', 'base'].includes(name)) problems.add('embedded-or-base-content');
    if (name === 'meta' && attrs.has('http-equiv')) problems.add('http-equiv');
    for (const attr of ['src', 'href', 'srcset', 'imagesrcset', 'action', 'formaction']) {
      const value = attrs.get(attr);
      if (!value) continue;
      if (/^\s*javascript:/i.test(value)) problems.add('javascript-url');
      // Ordinary <a href> navigation is allowed; every <link> (including
      // preconnect / dns-prefetch, which can reach third parties outside
      // CSP) and every resource attribute must stay local.
      if (attr === 'href' && name !== 'link') continue;
      const urls = attr.endsWith('srcset') ? value.split(',').map(candidate => candidate.trim().split(/\s+/)[0] ?? '') : [value];
      if (urls.some(url => /^[a-z][a-z0-9+.-]*:|^[/\\]{2}/i.test(url.trim()) || url.includes('\\'))) problems.add('nonlocal-resource');
      if (script || linkedResource) localReferences.add(value);
    }
    for (const child of node.childNodes ?? []) visit(child);
    if (node.content) visit(node.content);
  };
  visit(parse(html));
  return { problems: [...problems], localReferences: [...localReferences] };
}

// WebAssembly in the export (the terminal core) only compiles when script-src
// carries 'wasm-unsafe-eval'. Fail the deploy gate instead of shipping a build
// whose terminals cannot load; never accept general 'unsafe-eval'.
export function wasmPolicyProblems(assetPaths, csp) {
  const scriptSrc = (csp.split(';').map(part => part.trim()).find(part => part.startsWith('script-src ')) ?? '').split(/\s+/);
  const problems = [];
  if (scriptSrc.includes("'unsafe-eval'")) problems.push("script-src must never allow 'unsafe-eval'");
  if (assetPaths.some(path => path.endsWith('.wasm')) && !scriptSrc.includes("'wasm-unsafe-eval'")) {
    problems.push("export ships WebAssembly but script-src lacks 'wasm-unsafe-eval'");
  }
  return problems;
}

// Deployment config is an allowlist: any other top-level key (for example
// `tail_consumers`, which streams every request's metadata to another Worker,
// analytics/queue/storage bindings, or an `env.*` block that could re-enable
// observability for `wrangler deploy --env …`) fails the gate.
export const ALLOWED_CONFIG_KEYS = ['$schema', 'name', 'main', 'compatibility_date', 'workers_dev', 'preview_urls', 'routes',
  'assets', 'durable_objects', 'migrations', 'observability', 'send_metrics', 'logpush', 'upload_source_maps'];
export function configKeyProblems(config) {
  return Object.keys(config).filter(key => !ALLOWED_CONFIG_KEYS.includes(key)).map(key => `wrangler.jsonc: unexpected top-level key "${key}"`);
}

// SVG/XML documents are scriptable when navigated to directly: the export
// must not ship any with scripts, event handlers or javascript: URLs.
export function auditSvg(text) {
  const problems = [];
  if (/<script[\s>]/i.test(text)) problems.push('svg-script');
  if (/\son[a-z]+\s*=/i.test(text)) problems.push('svg-handler');
  if (/javascript:/i.test(text)) problems.push('javascript-url');
  if (/<foreignObject[\s>]/i.test(text)) problems.push('svg-foreign-object');
  return problems;
}
