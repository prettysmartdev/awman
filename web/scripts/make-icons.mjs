// Regenerates public/icons/*.png (run manually: `node scripts/make-icons.mjs`).
// Self-contained rasteriser so the icons are reproducible without image tools:
// a dark tile with a terminal prompt mark (">_"), matching public/icons/icon.svg.
import { writeFileSync } from 'node:fs';
import { deflateSync } from 'node:zlib';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const outDir = join(dirname(fileURLToPath(import.meta.url)), '..', 'public', 'icons');
const BG = [0x16, 0x17, 0x1a];
const FG = [0x7d, 0xd3, 0xa8];

function crc32(buf) {
  let c = ~0;
  for (const byte of buf) {
    c ^= byte;
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1));
  }
  return ~c >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function png(size, pixel) {
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    for (let x = 0; x < size; x++) raw.set(pixel(x, y), y * (size * 4 + 1) + 1 + x * 4);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr.set([8, 6, 0, 0, 0], 8);
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

function segDist(px, py, ax, ay, bx, by) {
  const dx = bx - ax;
  const dy = by - ay;
  const t = Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)));
  return Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
}

// Unit-square geometry (0..1). `inset` shrinks the mark for maskable safe zone.
function sample(u, v, { rounded, inset }) {
  if (rounded) {
    const r = 0.18;
    const cx = Math.min(Math.max(u, r), 1 - r);
    const cy = Math.min(Math.max(v, r), 1 - r);
    if (Math.hypot(u - cx, v - cy) > r) return null;
  }
  const s = 1 - 2 * inset;
  const mu = (u - inset) / s;
  const mv = (v - inset) / s;
  const w = 0.075;
  const chevron = Math.min(segDist(mu, mv, 0.22, 0.28, 0.48, 0.5), segDist(mu, mv, 0.48, 0.5, 0.22, 0.72));
  const underscore = segDist(mu, mv, 0.56, 0.72, 0.8, 0.72);
  return Math.min(chevron, underscore) < w ? FG : BG;
}

function render(size, opts) {
  const ss = 4;
  return png(size, (x, y) => {
    let r = 0, g = 0, b = 0, a = 0;
    for (let i = 0; i < ss; i++) {
      for (let j = 0; j < ss; j++) {
        const c = sample((x + (i + 0.5) / ss) / size, (y + (j + 0.5) / ss) / size, opts);
        if (!c) continue;
        r += c[0]; g += c[1]; b += c[2]; a += 1;
      }
    }
    const n = ss * ss;
    return a === 0 ? [0, 0, 0, 0] : [Math.round(r / a), Math.round(g / a), Math.round(b / a), Math.round((255 * a) / n)];
  });
}

for (const size of [192, 512]) {
  writeFileSync(join(outDir, `icon-${size}.png`), render(size, { rounded: true, inset: 0 }));
  writeFileSync(join(outDir, `maskable-${size}.png`), render(size, { rounded: false, inset: 0.1 }));
}
process.stdout.write(`wrote icons to ${outDir}\n`);
