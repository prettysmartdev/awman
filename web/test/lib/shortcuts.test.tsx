// @vitest-environment jsdom
import { useLayoutEffect } from 'react';
import { afterEach, expect, it, vi } from 'vitest';
import { cleanup, render } from '@testing-library/react';
import { useShortcuts } from '@/lib/keys/useShortcuts';

afterEach(cleanup);

it('dispatches the first key after a committed update using the current scope and handler', () => {
  const action = vi.fn();
  function Shortcuts({ ready }: { ready: boolean }) {
    useShortcuts(() => ({ context: ready ? 'composer' : 'global' }), mapped => {
      action(ready, mapped);
      return true;
    });
    // A focus/keyboard integration can dispatch synchronously after DOM commit,
    // before passive effects run. It must see the just-rendered state.
    useLayoutEffect(() => {
      if (ready) document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true }));
    }, [ready]);
    return null;
  }
  const view = render(<Shortcuts ready={false} />);
  view.rerender(<Shortcuts ready />);
  expect(action).toHaveBeenCalledExactlyOnceWith(true, 'composer.completeNext');
});
