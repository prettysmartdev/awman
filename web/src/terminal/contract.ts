// Terminal component contract (design §5.5). Written by the foundation lane;
// implemented by the terminal lane (src/terminal/** except this file).
// Screens render terminals ONLY through `TerminalView` re-exported here and
// never import the renderer (wterm/xterm) directly.
//
// Rules for implementations:
// - the renderer is fed only `connection.onSnapshot` (reset + write) and
//   `connection.onOutput`; input is never echoed locally;
// - `connection.resize` only while `control.holder === 'me'`;
// - disabled: OSC 52 (clipboard), OSC 7, OSC 9/777/1337, DCS/APC/Sixel/Kitty
//   graphics, hyperlink auto-open, WebMCP, renderer transport/shell/Markdown;
// - OSC 8 targets and plain URLs go to `onLinkDetected`, opened only from a
//   visible list on click, http(s) only, rel="noopener noreferrer";
// - clipboard writes only from `copySelection()` inside a user gesture;
// - paste: bracketed when the agent enabled mode 2004; >64 KiB chunked by the
//   connection; >1 MiB refused with a visible message.
import type { ForwardRefExoticComponent, RefAttributes } from 'react';
import type { TerminalConnection, TerminalControl } from '@/lib/client/terminalSocket';

export type { TerminalConnection, TerminalControl };

export interface DetectedLink {
  /** Only http: and https: URLs are ever reported. */
  url: string;
  /** Visible text the link was attached to (OSC 8) or the URL itself. */
  text: string;
  source: 'osc8' | 'text';
}

export interface TerminalViewProps {
  /** From the typed client; the view never opens sockets. */
  connection: TerminalConnection;
  /** From the InstanceSnapshot slot (agent, step, container). */
  title: string;
  /** Keyboard capture only when true. */
  focused: boolean;
  control: TerminalControl;
  /** 'Take control' button / first keypress when nobody holds control. */
  onRequestControl(): void;
  /** OSC 8 + plain URLs; never auto-opened. */
  onLinkDetected(links: DetectedLink[]): void;
  /** OSC 0/2 text only, sanitised; no navigation. */
  onTitleChange?(title: string): void;
  /** Bounded scrollback (default 10 000, max 50 000). */
  scrollbackLines: number;
  /** Show the ctrl/esc/arrows/paste toolbar. */
  touch: boolean;
  reducedMotion: boolean;
  theme: 'light' | 'dark';
}

export interface TerminalViewHandle {
  fit(): void;
  focus(): void;
  /** User gesture only. */
  copySelection(): Promise<void>;
  paste(text: string): void;
  reset(): void;
}

export type TerminalViewComponent = ForwardRefExoticComponent<TerminalViewProps & RefAttributes<TerminalViewHandle>>;

export const DEFAULT_SCROLLBACK_LINES = 10_000;
export const MAX_SCROLLBACK_LINES = 50_000;

export interface KeyModes {
  /** DECCKM application cursor keys. */
  appCursor: boolean;
  /** Mode 2004 bracketed paste. */
  bracketedPaste: boolean;
}

export type MouseMode = 'none' | 'x10' | 'normal' | 'button' | 'any';

export interface MouseLike {
  kind: 'down' | 'up' | 'move' | 'wheel';
  button: 0 | 1 | 2 | 3;
  col: number; // 1-based cell
  row: number; // 1-based cell
  shift: boolean;
  alt: boolean;
  ctrl: boolean;
  wheelDelta?: number;
  sgr: boolean;
}

/** Port of src/frontend/tui/key_handler::key_to_bytes. */
export type EncodeKey = (event: KeyboardEvent, modes: KeyModes) => Uint8Array | null;
/** X10/normal/button/any + SGR mouse reports. */
export type EncodeMouse = (event: MouseLike, mode: MouseMode) => Uint8Array | null;

export { TerminalView } from './TerminalView';
export { encodeKey, encodeMouse } from './input/encoder';
