'use client';

// Copyable owner-side commands. Clipboard writes happen only from the user's
// click (never automatically).
import { useState } from 'react';

export function CommandBlock({ commands, label, primary = false }: { commands: readonly string[]; label: string; primary?: boolean }) {
  const [result, setResult] = useState<{ text: string; copied: boolean } | null>(null);
  const text = commands.join('\n');
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setResult({ text, copied: true });
    } catch {
      setResult({ text, copied: false });
    }
  };
  return (
    <div className="command-block">
      <pre aria-label={label} tabIndex={0}>
        {commands.map(command => (
          <code key={command}>{command}</code>
        ))}
      </pre>
      <button type="button" className={primary ? undefined : 'secondary'} onClick={copy} aria-label={`Copy ${label.toLowerCase()}`}>
        {result?.text === text && result.copied ? 'Copied' : 'Copy'}
      </button>
      <span className="copy-result hint" role="status">{result?.text === text ? result.copied ? `${label} copied.` : 'Could not copy. Select the command and copy it manually.' : ''}</span>
    </div>
  );
}
