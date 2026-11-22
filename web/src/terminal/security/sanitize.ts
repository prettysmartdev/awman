// Untrusted terminal metadata (design §5.5): agent output, OSC titles and
// hyperlink targets are text, never markup or navigation. Everything here is
// pure so the fuzz test can exercise it directly.

export const MAX_TITLE_CHARS = 120;
export const MAX_LINK_CHARS = 2048;
export const MAX_DETECTED_LINKS = 50;

// Invisible/format code points that can disguise text: zero-width chars and
// directional marks, line/paragraph separators, bidi embeddings/overrides,
// word joiner and bidi isolates, BOM, interlinear annotation controls.
const FORMAT_RANGES: readonly (readonly [number, number])[] = [
  [0x200b, 0x200f],
  [0x2028, 0x202e],
  [0x2060, 0x2069],
  [0xfeff, 0xfeff],
  [0xfff9, 0xfffb],
];

/** C0/C1 controls, DEL and the invisible format characters above. */
export function isUnsafeCodePoint(cp: number): boolean {
  if (cp <= 0x1f || (cp >= 0x7f && cp <= 0x9f)) return true;
  return FORMAT_RANGES.some(([lo, hi]) => cp >= lo && cp <= hi);
}

function stripUnsafe(text: string): string {
  let out = '';
  for (const ch of text) if (!isUnsafeCodePoint(ch.codePointAt(0) ?? 0)) out += ch;
  return out;
}

/** OSC 0/2 title → plain text: no controls, no bidi tricks, bounded. */
export function sanitizeTitle(raw: string | null | undefined): string {
  if (!raw) return '';
  const clean = stripUnsafe(raw.slice(0, 4 * MAX_TITLE_CHARS)).replace(/\s+/g, ' ').trim();
  const chars = [...clean];
  return chars.length > MAX_TITLE_CHARS ? `${chars.slice(0, MAX_TITLE_CHARS - 1).join('')}…` : clean;
}

/**
 * Vet a link target before it is ever shown as clickable: absolute http(s)
 * only, no embedded credentials, no whitespace/controls/format characters,
 * bounded length. Returns the normalised href or null.
 */
export function vetLink(raw: string | null | undefined): string | null {
  if (!raw || raw.length > MAX_LINK_CHARS) return null;
  for (const ch of raw) {
    const cp = ch.codePointAt(0) ?? 0;
    if (cp === 0x20 || isUnsafeCodePoint(cp)) return null;
  }
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return null;
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') return null;
  if (url.username || url.password || !url.hostname) return null;
  return url.href.length > MAX_LINK_CHARS ? null : url.href;
}

// eslint-disable-next-line no-control-regex -- URL scan stops at controls
const URL_RE = /\bhttps?:\/\/[^\s<>"'`{}|\\^\x00-\x1f\x7f]+/gi;

/** Plain-text URLs in a line of rendered output (trailing punctuation trimmed, vetted). */
export function findUrls(text: string): string[] {
  const out: string[] = [];
  for (const match of text.matchAll(URL_RE)) {
    let candidate = match[0];
    // Drop sentence punctuation and an unbalanced closing bracket.
    while (/[.,;:!?)\]]$/.test(candidate)) {
      const last = candidate[candidate.length - 1];
      const opens = candidate.split(last === ')' ? '(' : '[').length - 1;
      const closes = candidate.split(last === ')' ? ')' : ']').length - 1;
      if ((last === ')' || last === ']') && opens >= closes) break;
      candidate = candidate.slice(0, -1);
    }
    const vetted = vetLink(candidate);
    if (vetted) out.push(vetted);
  }
  return out;
}
