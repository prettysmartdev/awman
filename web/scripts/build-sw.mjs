// Generates out/sw.js from src/sw/sw.ts after `next build` + externalize-inline
// (design §5.8). The precache list is exactly the exported shell:
//   "/" (index.html), 404.html, /_next/static/**, /icons/**, manifest.
// The version is a SHA-256 over every precached file's path and bytes, so any
// shell change produces a new cache and an atomic update.
//
// Also stamps the same version into out/manifest.webmanifest
// ("version" member) before hashing, so the installed app reports which shell
// it is running.
import { createHash } from 'node:crypto';
import { readFile, readdir, writeFile } from 'node:fs/promises';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');

async function* walk(dir) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else if (entry.isFile()) yield path;
  }
}

/** Which exported files belong in the offline shell. */
export function isShellFile(file) {
  if (file === 'index.html' || file === '404.html' || file === 'manifest.webmanifest') return true;
  if (file.startsWith('_next/static/')) return !file.endsWith('.map');
  if (file.startsWith('icons/')) return true;
  return false;
}

export function urlFor(file) {
  return file === 'index.html' ? '/' : `/${file}`;
}

async function shellFiles(outDir, exclude = '') {
  const files = [];
  for await (const path of walk(outDir)) {
    const file = relative(outDir, path).split(sep).join('/');
    if (isShellFile(file) && file !== exclude) files.push(file);
  }
  return files.sort();
}

async function contentVersion(outDir, files) {
  const hash = createHash('sha256');
  for (const file of files) hash.update(file).update('\0').update(await readFile(join(outDir, file))).update('\0');
  return hash.digest('hex').slice(0, 16);
}

export async function precacheManifest(outDir) {
  const files = await shellFiles(outDir);
  return { version: await contentVersion(outDir, files), urls: files.map(urlFor) };
}

async function stampManifestVersion(outDir) {
  const path = join(outDir, 'manifest.webmanifest');
  const manifest = JSON.parse(await readFile(path, 'utf8'));
  // Version of the shell excluding the manifest itself (avoids a cycle).
  manifest.version = await contentVersion(outDir, await shellFiles(outDir, 'manifest.webmanifest'));
  await writeFile(path, `${JSON.stringify(manifest, null, 2)}\n`);
  return manifest.version;
}

export async function buildServiceWorker(outDir) {
  const shellVersion = await stampManifestVersion(outDir);
  const manifest = await precacheManifest(outDir);
  if (!manifest.urls.includes('/')) throw new Error('index.html missing from export');
  await build({
    entryPoints: [join(webRoot, 'src/sw/sw.ts')],
    outfile: join(outDir, 'sw.js'),
    bundle: true,
    format: 'iife',
    target: 'es2022',
    platform: 'browser',
    minify: true,
    sourcemap: false,
    legalComments: 'none',
    define: { __AWMAN_PRECACHE__: JSON.stringify(manifest) },
    logLevel: 'warning',
  });
  return { ...manifest, shellVersion };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const outDir = resolve(process.argv[2] ?? 'out');
  try {
    const { version, urls } = await buildServiceWorker(outDir);
    process.stdout.write(`build-sw: out/sw.js version ${version}, ${urls.length} precached shell files\n`);
  } catch (error) {
    process.stderr.write(`build-sw failed: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
