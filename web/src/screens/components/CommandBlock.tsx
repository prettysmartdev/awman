'use client';

// Copyable owner-side commands. Clipboard writes happen only from the user's
// click (never automatically).
import { useState } from 'react';

export function CommandBlock({ commands, label }: { commands: readonly string[]; label: string }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(commands.join('\n'));
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      setCopied(false);
    }
  };
  return (
    <div className="command-block">
      <pre aria-label={label}>
        {commands.map(command => (
          <code key={command}>{command}</code>
        ))}
      </pre>
      <button type="button" className="secondary" onClick={copy} aria-label={`Copy ${label.toLowerCase()}`}>
        {copied ? 'Copied' : 'Copy'}
      </button>
    </div>
  );
}
