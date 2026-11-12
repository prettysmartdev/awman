// Built-output audit (design §5.1, §5.7; gaps WF-1/WF-4). Fails when the
// static export:
//   - names any network origin other than the hosted app origin and the fixed
//     same-machine daemon (exactly `http://127.0.0.1:9877` and
//     `ws://127.0.0.1:9877`, WI 0159 §4, in shipped JavaScript only), outside
//     a short list of inert strings (XML namespaces, framework
//     error-documentation links, core-js URL feature tests) that are never
//     fetched. A TLS variant, another port/host, or a page-derived daemon
//     host fails. The production CSP `connect-src` is the runtime
//     enforcement; this is the build-time tripwire for an accidental cloud
//     request or a moved daemon destination.
//   - uses browser persistent storage APIs (localStorage, indexedDB,
//     document.cookie) anywhere in shipped code. The single exception (WI
//     0160 C4) is the saved-key database open call
//     `indexedDB.open("awman-browser-auth",1)`, admitted in at most one
//     JavaScript file; any other IndexedDB use still fails;
//   - contains anything shaped like session data or credentials in HTML/RSC
//     payloads (grant/ticket tokens, polling secrets, instance/terminal ids,
//     home-directory paths);
//   - ships source maps.
import { readFile, readdir } from 'node:fs/promises';
import { extname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

export const ALLOWED_ORIGINS = ['https://awman.prettysmart.dev'];
/** The fixed daemon destinations (web/src/lib/client/http.ts, edge CSP connect-src). */
export const DAEMON_ORIGINS = ['http://127.0.0.1:9877', 'ws://127.0.0.1:9877'];

// Exact strings or prefixes that appear in framework code as data, not as
// request targets. Prefixes end with '/'.
const INERT = [
  'http://www.w3.org/1999/xlink',
  'http://www.w3.org/2000/svg',
  'http://www.w3.org/1999/xhtml',
  'http://www.w3.org/XML/1998/namespace',
  'http://www.w3.org/1998/Math/MathML',
  'https://react.dev/errors/',
  'https://nextjs.org/docs/',
  // Dummy bases Next's router passes to `new URL(path, base)` to parse paths.
  'http://n',
  'http://f',
];
// core-js (Next's legacy-browser polyfills, `nomodule` only) probes URL
// parsing with these literals; they are never fetched.
const POLYFILL_INERT = [
  'http://n', 'http://f', 'https://x', 'https://a', 'https://a@b', 'https://a/c%20d?a=1&c=3', 'https://a#б', 'https://тест',
  'https://github.com/zloirock/core-js',
];

const TEXT_EXT = new Set(['.html', '.js', '.mjs', '.css', '.json', '.txt', '.webmanifest', '.svg', '.xml']);
const URL_RE = /\b(?:https?|wss?):\/\/[^\s"'`()<>\\]*/gi;
const STORAGE_RE = /\b(?:localStorage|indexedDB)\b|document\.cookie/;
// The one permitted IndexedDB access: opening the credential database.
const CREDENTIAL_DB_OPEN_RE = /\bindexedDB\??\.open\(\s*["']awman-browser-auth["']\s*,\s*1\s*\)/g;
const SESSION_RES = [
  [/awb_[A-Za-z0-9_-]{16,}/, 'browser grant token'],
  [/awt_[A-Za-z0-9_-]{16,}/, 'upgrade ticket'],
  [/polling_secret"?\s*:\s*"[A-Za-z0-9_-]{16,}/, 'polling secret'],
  [/\b(?:blk|brw|ins|lc|term|tab|frn)_[0-9a-f]{32}\b/, 'daemon resource id'],
  [/(?:\/Users\/|\/home\/)[A-Za-z0-9._-]+\//, 'home-directory path'],
];

function originOf(url) {
  try {
    return new URL(url).origin;
  } catch {
    return null;
  }
}

function urlAllowed(url, file) {
  const trimmed = url.replace(/[.,;:]+$/, '');
  if (ALLOWED_ORIGINS.includes(originOf(trimmed) ?? '')) return true;
  if (file.endsWith('.svg')) return trimmed === 'http://www.w3.org/2000/svg' || trimmed === 'http://www.w3.org/1999/xlink';
  if (!file.endsWith('.js')) return false;
  // Exactly the daemon origin itself (optionally with a path); never userinfo or another port.
  if (DAEMON_ORIGINS.some(daemon => trimmed === daemon || trimmed.startsWith(`${daemon}/`))) return true;
  if (INERT.some(entry => (entry.endsWith('/') ? trimmed.startsWith(entry) : trimmed === entry))) return true;
  if (/(?:^|\/)polyfills-[0-9a-f]+\.js$/.test(file)) {
    return POLYFILL_INERT.some(entry => trimmed === entry || trimmed.startsWith(`${entry}/`));
  }
  return false;
}

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else if (entry.isFile()) yield path;
  }
}

/** Returns a list of human-readable failures (empty = pass). */
export async function auditOutput(outDir) {
  const failures = [];
  let sawIndex = false;
  const credentialDbFiles = [];
  for await (const path of walk(outDir)) {
    const file = relative(outDir, path).split(sep).join('/');
    if (file === 'index.html') sawIndex = true;
    if (file.endsWith('.map')) {
      failures.push(`${file}: source map shipped`);
      continue;
    }
    if (!TEXT_EXT.has(extname(file))) continue;
    const text = await readFile(path, 'utf8');
    for (const match of text.matchAll(URL_RE)) {
      if (!urlAllowed(match[0], file)) failures.push(`${file}: disallowed network origin ${JSON.stringify(match[0].slice(0, 80))}`);
    }
    const opens = /\.(?:js|mjs)$/.test(file) ? (text.match(CREDENTIAL_DB_OPEN_RE) ?? []).length : 0;
    if (opens) credentialDbFiles.push(file);
    if (opens > 1) failures.push(`${file}: opens the credential database more than once`);
    const scanned = opens ? text.replace(CREDENTIAL_DB_OPEN_RE, '') : text;
    if (/\.(?:js|mjs|html)$/.test(file) && STORAGE_RE.test(scanned)) failures.push(`${file}: uses browser persistent storage (localStorage/indexedDB/cookie)`);
    if (/\.(?:html|txt|json|webmanifest)$/.test(file) || file.startsWith('_next/static/inline/')) {
      for (const [re, what] of SESSION_RES) if (re.test(text)) failures.push(`${file}: contains ${what}`);
    }
  }
  if (credentialDbFiles.length > 1) failures.push(`credential database opened from several files: ${credentialDbFiles.join(', ')}`);
  if (!sawIndex) failures.push('index.html missing');
  return failures;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const outDir = resolve(process.argv[2] ?? 'out');
  const failures = await auditOutput(outDir).catch(error => [String(error)]);
  if (failures.length) {
    process.stderr.write(`audit-output: ${failures.length} problem(s)\n${[...new Set(failures)].join('\n')}\n`);
    process.exitCode = 1;
  } else {
    process.stdout.write('audit-output: only the hosted origin and fixed loopback daemon; no storage APIs, session data or source maps in the export\n');
  }
}
