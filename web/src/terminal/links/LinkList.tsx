'use client';

// The only place terminal links become clickable (design §5.5): a visible
// list the user clicks. Targets are re-vetted here (http/https, no
// credentials) so a caller cannot pass an unvetted URL through.
import type { DetectedLink } from '../contract';
import { vetLink } from '../security/sanitize';

export interface LinkListProps {
  links: readonly DetectedLink[];
  className?: string;
}

export function LinkList({ links, className }: LinkListProps) {
  const safe = links.flatMap(link => {
    const url = vetLink(link.url);
    return url ? [{ ...link, url }] : [];
  });
  if (safe.length === 0) return null;
  return (
    <details className={className}>
      <summary>Links in this terminal ({safe.length})</summary>
      <ul>
        {safe.map(link => (
          <li key={`${link.source}:${link.url}`}>
            <a href={link.url} target="_blank" rel="noopener noreferrer">
              {link.text === link.url ? link.url : `${link.text} — ${link.url}`}
            </a>
          </li>
        ))}
      </ul>
    </details>
  );
}
