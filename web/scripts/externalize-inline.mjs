// Post-export step (design §5.8): the edge CSP is `script-src 'self'` with no
// 'unsafe-inline' and no hashes, so every inline <script> that Next's static
// export emits (the React Server Components flight pushes) is moved, in place
// and in order, to an external content-addressed file under
// /_next/static/inline/. Parser-blocking external scripts execute in document
// order, which preserves the semantics of the inline ones they replace.
//
// Anything we cannot safely externalize (non-JS script types, inline handlers)
// is a hard failure: we never weaken the CSP instead.
import { createHash } from 'node:crypto';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT = /<script\b([^>]*)>([\s\S]*?)<\/script>/gi;
const JS_TYPES = new Set(['', 'text/javascript', 'application/javascript', 'module']);

async function* htmlFiles(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* htmlFiles(path);
    else if (entry.isFile() && entry.name.endsWith('.html')) yield path;
  }
}

function attribute(attrs, name) {
  const match = new RegExp(`(?:^|\\s)${name}\\s*=\\s*(?:"([^"]*)"|'([^']*)'|([^\\s>]+))`, 'i').exec(attrs);
  if (match) return match[1] ?? match[2] ?? match[3] ?? '';
  return new RegExp(`(?:^|\\s)${name}(?:\\s|/|$)`, 'i').test(attrs) ? '' : null;
}

/** Rewrites every HTML file under `outDir`; returns counts. Throws on anything it cannot move. */
export async function externalize(outDir) {
  const inlineDir = join(outDir, '_next', 'static', 'inline');
  const written = new Set();
  const failures = [];
  let moved = 0;
  await mkdir(inlineDir, { recursive: true });
  for await (const file of htmlFiles(outDir)) {
    const html = await readFile(file, 'utf8');
    const pending = [];
    const rewritten = html.replace(SCRIPT, (whole, attrs, body) => {
      const where = relative(outDir, file);
      if (/\son[a-z]+\s*=/i.test(attrs)) failures.push(`${where}: inline handler on <script>`);
      if (attribute(attrs, 'src') !== null) {
        if (body.trim()) failures.push(`${where}: <script> with both src and an inline body`);
        return whole;
      }
      const type = (attribute(attrs, 'type') ?? '').trim().toLowerCase();
      if (!JS_TYPES.has(type)) {
        failures.push(`${where}: inline <script type="${type}"> cannot be externalized`);
        return whole;
      }
      const name = `${createHash('sha256').update(body).digest('hex').slice(0, 20)}.js`;
      if (!written.has(name)) {
        written.add(name);
        pending.push([name, body]);
      }
      moved += 1;
      return `<script src="/_next/static/inline/${name}"${type === 'module' ? ' type="module"' : ''}></script>`;
    });
    for (const [name, body] of pending) await writeFile(join(inlineDir, name), body);
    if (rewritten !== html) await writeFile(file, rewritten);
  }
  if (failures.length) throw new Error(failures.join('\n'));
  return { moved, files: written.size, inlineDir };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const outDir = resolve(process.argv[2] ?? 'out');
  try {
    const { moved, files, inlineDir } = await externalize(outDir);
    process.stdout.write(
      `externalize-inline: moved ${moved} inline scripts into ${files} files under ${relative(process.cwd(), inlineDir).split(sep).join('/')}\n`,
    );
  } catch (error) {
    process.stderr.write(`externalize-inline failed:\n${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
