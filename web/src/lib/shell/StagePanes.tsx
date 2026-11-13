'use client';
import { useLayoutEffect, useRef, useState, type ReactNode } from 'react';

/** Keep both surfaces mounted so minimizing never disconnects a terminal. */
export function StagePanes({ execution, containers, executionOpen, containersOpen, minimized, inert }: {
  execution: ReactNode;
  containers: ReactNode;
  executionOpen: boolean;
  containersOpen: boolean;
  minimized: ReactNode;
  inert: boolean;
}) {
  const panes = useRef<HTMLDivElement>(null);
  const divider = useRef<HTMLDivElement>(null);
  const [share, setShare] = useState(1 / 3);
  const [drag, setDrag] = useState<'mouse' | number | null>(null);
  useLayoutEffect(() => {
    // CSSOM property updates are allowed by the CSP; inline style attributes are not.
    panes.current?.style.setProperty('--execution-share', `${share}fr`);
    panes.current?.style.setProperty('--container-share', `${1 - share}fr`);
  }, [share]);
  const resize = (value: number) => setShare(Math.max(0.2, Math.min(0.8, value)));
  useLayoutEffect(() => {
    if (drag === null) return;
    const move = (x: number) => {
      const box = panes.current?.getBoundingClientRect();
      const handle = divider.current?.getBoundingClientRect();
      if (box && handle && box.width > handle.width) resize((x - box.left - handle.width / 2) / (box.width - handle.width));
    };
    const stop = () => setDrag(null);
    const mouseMove = (event: MouseEvent) => { if (drag === 'mouse') move(event.clientX); };
    const mouseUp = () => { if (drag === 'mouse') stop(); };
    const touchMove = (event: TouchEvent) => {
      const touch = Array.from(event.changedTouches).find(t => t.identifier === drag);
      if (touch) { event.preventDefault(); move(touch.clientX); }
    };
    const touchEnd = (event: TouchEvent) => { if (Array.from(event.changedTouches).some(t => t.identifier === drag)) stop(); };
    // Track outside the handle as well, including browsers that omit pointer
    // events during touch emulation. Cleanup also covers unmount and lost focus.
    document.addEventListener('mousemove', mouseMove);
    document.addEventListener('mouseup', mouseUp);
    document.addEventListener('touchmove', touchMove, { passive: false });
    document.addEventListener('touchend', touchEnd);
    document.addEventListener('touchcancel', touchEnd);
    window.addEventListener('blur', stop);
    return () => {
      document.removeEventListener('mousemove', mouseMove);
      document.removeEventListener('mouseup', mouseUp);
      document.removeEventListener('touchmove', touchMove);
      document.removeEventListener('touchend', touchEnd);
      document.removeEventListener('touchcancel', touchEnd);
      window.removeEventListener('blur', stop);
    };
  }, [drag]);
  return <>
    <div ref={panes} className="stage-panes" data-execution-open={executionOpen} data-containers-open={containersOpen} data-resizing={drag !== null}>
      <section className="shell-execution" aria-label="Execution area" inert={inert}>{execution}</section>
      <div ref={divider} className="pane-divider" role="separator" aria-label="Resize execution and container panels" aria-orientation="vertical" aria-valuemin={20} aria-valuemax={80} aria-valuenow={Math.round(share * 100)} aria-valuetext={`Execution ${Math.round(share * 100)}%, container ${Math.round((1 - share) * 100)}%`} tabIndex={inert ? -1 : 0} inert={inert}
        onMouseDown={event => {
          if (event.button !== 0) return;
          event.preventDefault(); event.currentTarget.focus();
          setDrag('mouse');
        }}
        onTouchStart={event => {
          const touch = event.changedTouches[0];
          if (touch && drag === null) { event.currentTarget.focus(); setDrag(touch.identifier); }
        }}
        onDoubleClick={() => setShare(1 / 3)}
        onKeyDown={event => {
          const next = event.key === 'ArrowLeft' ? share - 0.02 : event.key === 'ArrowRight' ? share + 0.02 : event.key === 'Home' ? 0.2 : event.key === 'End' ? 0.8 : event.key === 'Enter' ? 1 / 3 : null;
          if (next === null) return;
          event.preventDefault(); event.stopPropagation(); resize(next);
        }} />
      <section className="shell-containers" aria-label="Agents" inert={inert}>{containers}</section>
      {!executionOpen && !containersOpen && <p className="panes-empty">Restore a panel from the bar below.</p>}
    </div>
    {minimized && <nav className="minimized-panels" aria-label="Minimized panels" inert={inert}>{minimized}</nav>}
  </>;
}
