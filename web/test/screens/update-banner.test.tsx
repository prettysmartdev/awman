// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import type { ShellUpdate } from '@/lib/pwa/serviceWorker';
const captured = vi.hoisted(() => ({ ready: null as ((update: ShellUpdate) => void) | null }));
vi.mock('@/lib/connect/useConnect', () => ({ useConnect: () => ({ state: { phase: 'probing', previous: null }, controller: { reset: () => undefined } }) }));
vi.mock('@/screens/connect/ConnectScreen', () => ({ ConnectScreen: () => <main><header className="shell-app-bar">App bar</header></main> }));
vi.mock('@/lib/pwa/serviceWorker', () => ({ registerShellWorker: (ready: (update: ShellUpdate) => void) => { captured.ready = ready; return () => undefined; } }));
import App from '@/app/App';
afterEach(cleanup);

it('shows the update banner in normal flow before the app, applies on Reload and hides on Later until a newer update (B10)', () => {
  const { container } = render(<App />);
  const apply = vi.fn();
  act(() => captured.ready?.({ apply }));
  const banner = screen.getByRole('status');
  expect(banner.textContent).toContain('Update available.');
  expect(container.firstElementChild).toBe(banner); // in flow, before the app bar
  fireEvent.click(screen.getByRole('button', { name: 'Reload to update' })); expect(apply).toHaveBeenCalledTimes(1);
  fireEvent.click(screen.getByRole('button', { name: 'Later' }));
  expect(screen.queryByText('Update available.')).toBeNull();
  act(() => captured.ready?.({ apply: vi.fn() }));
  expect(screen.getByText('Update available.')).toBeTruthy();
});
