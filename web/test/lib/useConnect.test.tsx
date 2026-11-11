// @vitest-environment jsdom
import { StrictMode } from 'react';
import { act, cleanup, render } from '@testing-library/react';
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
