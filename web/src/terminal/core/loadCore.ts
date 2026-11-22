// Terminal parser core (design §5.5.1): wterm's Ghostty core (`@wterm/ghostty`,
// libghostty-vt compiled to WASM). Only the headless core is used — never
// `@wterm/dom`/`@wterm/react` (their row renderer writes inline `style=`
// markup, which the production CSP blocks, and emits clickable OSC 8 anchors),
// nor `@wterm/core`'s WebSocketTransport, shell, Markdown or WebMCP pieces.
//
// The `.wasm` ships self-hosted: the bundler rewrites the package's
// `new URL('../wasm/ghostty-vt.wasm', import.meta.url)` into a hashed file
// under `/_next/static/media/`, which the service worker precaches. Compiling
// it needs CSP `'wasm-unsafe-eval'` (never `'unsafe-eval'`).
import type { TerminalCore, TerminalThemeColors } from '@wterm/core';
import { MAX_SCROLLBACK_LINES } from '../contract';

export type { TerminalCore };

export interface CoreOptions {
  cols: number;
  rows: number;
  scrollbackLines: number;
}

/** Creates an initialised core; injectable so tests can load the WASM from disk. */
export type CoreFactory = (options: CoreOptions) => Promise<TerminalCore & { dispose?(): void }>;

export function clampScrollback(lines: number): number {
  if (!Number.isFinite(lines)) return 0;
  return Math.max(0, Math.min(MAX_SCROLLBACK_LINES, Math.floor(lines)));
}

export const loadGhosttyCore: CoreFactory = async ({ cols, rows, scrollbackLines }) => {
  // Code-split: the shell does not fetch the terminal core until a terminal is shown.
  const { GhosttyCore } = await import('@wterm/ghostty');
  const core = await GhosttyCore.load({
    scrollbackLines: clampScrollback(scrollbackLines),
    // Kitty graphics are disabled: untrusted output never decodes images.
    imageStorageLimit: 0,
  });
  core.init(cols, rows);
  return core;
};

const DARK: TerminalThemeColors = {
  foreground: 0xd4d4d4,
  background: 0x1b1b1f,
  cursor: 0xd4d4d4,
  palette: [0x1b1b1f, 0xe5534b, 0x57ab5a, 0xc69026, 0x539bf5, 0xb083f0, 0x39c5cf, 0xd4d4d4, 0x636e7b, 0xff938a, 0x6bc46d, 0xdaaa3f, 0x6cb6ff, 0xdcbdfb, 0x56d4dd, 0xffffff],
};
const LIGHT: TerminalThemeColors = {
  foreground: 0x1f2328,
  background: 0xffffff,
  cursor: 0x1f2328,
  palette: [0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781, 0x57606a, 0xa40e26, 0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f],
};

export function themeColors(theme: 'light' | 'dark'): TerminalThemeColors {
  return theme === 'light' ? LIGHT : DARK;
}
