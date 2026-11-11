// @vitest-environment jsdom
import { StrictMode } from 'react';
import { act, cleanup, render } from '@testing-library/react';
import { hydrateRoot } from 'react-dom/client';
import { renderToString } from 'react-dom/server';
import { afterEach, expect, it, vi } from 'vitest';
import { ConnectController } from '@/lib/connect/controller';
import { useConnect } from '@/lib/connect/useConnect';

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

it('starts once through Strict Mode effect replay and disposes on unmount', async () => {
  const start = vi.spyOn(ConnectController.prototype, 'start').mockResolvedValue();
  const dispose = vi.spyOn(ConnectController.prototype, 'dispose').mockImplementation(() => undefined);
  function Probe() { useConnect(); return null; }
  const result = render(<StrictMode><Probe /></StrictMode>);
  await act(async () => { await Promise.resolve(); });
  expect(start).toHaveBeenCalledTimes(1);
  expect(dispose).not.toHaveBeenCalled();
  result.unmount();
  await act(async () => { await Promise.resolve(); });
  expect(dispose).toHaveBeenCalledTimes(1);
});

it('hydrates without a changing server snapshot', async () => {
  vi.spyOn(ConnectController.prototype, 'start').mockResolvedValue();
  const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined);
  function Probe() { const { state } = useConnect(); return <span>{state.phase}</span>; }
  const container = document.createElement('div');
  container.innerHTML = renderToString(<Probe />);
  document.body.append(container);
  let root: ReturnType<typeof hydrateRoot> | null = null;
  try {
    await act(async () => { root = hydrateRoot(container, <Probe />); });
    expect(container.textContent).toBe('idle');
    expect(consoleError.mock.calls.some(call => String(call[0]).includes('getServerSnapshot should be cached'))).toBe(false);
  } finally {
    await act(async () => { root?.unmount(); });
    container.remove();
  }
});
