'use client';
import { useEffect, useRef, type ReactNode } from 'react';

/** Move focus only when the entry step changes, never on a background recheck. */
export function EntryFrame({ title, state, busy = false, children }: { title: string; state: string; busy?: boolean; children: ReactNode }) {
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => { heading.current?.focus(); }, [state]);
  return <main className="connect">
    <section className="connect-card" data-state={state} aria-labelledby="entry-title">
      <div className="entry-brand" aria-hidden="true">awman <span>On this machine</span></div>
      <header aria-busy={busy}><h1 id="entry-title" ref={heading} tabIndex={-1}>{title}</h1></header>
      {children}
    </section>
  </main>;
}
