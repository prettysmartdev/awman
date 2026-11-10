'use client';

// Control keys for touch devices (design §5.5, matrix term.*): Esc, Tab, a
// latching Ctrl, arrows, Ctrl+C and Paste. Buttons never take focus away from
// the terminal input (pointerdown is prevented) so the soft keyboard stays up.
import type { KeyLike } from './encoder';

export interface TouchToolbarProps {
  disabled: boolean;
  ctrlLatched: boolean;
  onToggleCtrl(): void;
  onKey(key: KeyLike): void;
  onPaste(): void;
  className?: string;
}

const key = (name: string, extra: Partial<KeyLike> = {}): KeyLike => ({ key: name, ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, ...extra });

const KEYS: { label: string; aria: string; key: KeyLike }[] = [
  { label: 'Esc', aria: 'Escape', key: key('Escape') },
  { label: 'Tab', aria: 'Tab', key: key('Tab') },
  { label: '←', aria: 'Left arrow', key: key('ArrowLeft') },
  { label: '↑', aria: 'Up arrow', key: key('ArrowUp') },
  { label: '↓', aria: 'Down arrow', key: key('ArrowDown') },
  { label: '→', aria: 'Right arrow', key: key('ArrowRight') },
  { label: '^C', aria: 'Control C (interrupt)', key: key('c', { ctrlKey: true }) },
];

export function TouchToolbar({ disabled, ctrlLatched, onToggleCtrl, onKey, onPaste, className }: TouchToolbarProps) {
  const keep = (event: { preventDefault(): void }) => event.preventDefault();
  return (
    <div className={className} role="toolbar" aria-label="Terminal keys">
      {KEYS.slice(0, 2).map(item => (
        <button key={item.aria} type="button" aria-label={item.aria} disabled={disabled} onPointerDown={keep} onClick={() => onKey(item.key)}>
          {item.label}
        </button>
      ))}
      <button type="button" aria-label="Control (applies to the next key)" aria-pressed={ctrlLatched} disabled={disabled} onPointerDown={keep} onClick={onToggleCtrl}>
        Ctrl
      </button>
      {KEYS.slice(2).map(item => (
        <button key={item.aria} type="button" aria-label={item.aria} disabled={disabled} onPointerDown={keep} onClick={() => onKey(item.key)}>
          {item.label}
        </button>
      ))}
      <button type="button" aria-label="Paste into the agent" disabled={disabled} onPointerDown={keep} onClick={onPaste}>
        Paste
      </button>
    </div>
  );
}
