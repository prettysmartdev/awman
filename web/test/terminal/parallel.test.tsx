// @vitest-environment jsdom
// Parallel terminals: each view owns its own core instance (shared compiled
// WASM module, separate memory) and renders only its own connection's frames,
// even when frames interleave; disposing one leaves the others intact.
import { act, cleanup, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';
import { createTerminalView } from '@/terminal/TerminalView';
import { FakeConnection, fixture, nodeCoreFactory, type Fixture } from './helpers';

const TerminalView = createTerminalView(nodeCoreFactory);
afterEach(() => cleanup());

const NAMES = ['alt_screen_redraw', 'utf8_split', 'colours', 'resize_storm'] as const;

function Grid({ conns, show }: { conns: FakeConnection[]; show: boolean[] }) {
  return (
    <div>
      {conns.map((conn, i) =>
        show[i] ? (
          <TerminalView
            key={i}
            connection={conn.asConnection()}
            title={`agent ${i}`}
            focused={i === 0}
            control={conn.control.get()}
            onRequestControl={() => undefined}
            onLinkDetected={() => undefined}
            scrollbackLines={1000}
            touch={false}
            reducedMotion
            theme="dark"
          />
        ) : null,
      )}
    </div>
  );
}

const rowsOf = (title: string) => [...screen.getByRole('log', { name: `${title} output` }).querySelectorAll('.term-row')].map(r => (r.textContent ?? '').trimEnd());

describe('parallel terminals', () => {
  it('four concurrent terminals render their own interleaved fixtures', async () => {
    const conns = NAMES.map(() => new FakeConnection({ holder: 'none', inputAllowed: true }));
    const fixtures: Fixture[] = NAMES.map(name => fixture(name));
    const view = render(<Grid conns={conns} show={[true, true, true, true]} />);
    await waitFor(() => expect(screen.queryAllByText('Loading terminal…')).toHaveLength(0));
    act(() => {
      const longest = Math.max(...fixtures.map(f => f.frames.length));
      for (let step = 0; step < longest; step++) {
        fixtures.forEach((fx, i) => {
          const frame = fx.frames[step];
          if (frame) conns[i]!.frame(frame);
        });
      }
    });
    await waitFor(() => fixtures.forEach((fx, i) => expect(rowsOf(`agent ${i}`)).toEqual(fx.expect.lines)));

    // Unmount terminal 1: the others keep rendering new output.
    view.rerender(<Grid conns={conns} show={[true, false, true, true]} />);
    act(() => conns[2]!.outputs.emit(new TextEncoder().encode('\r\nmore')));
    await waitFor(() => expect(rowsOf('agent 2')).toContain('more'));
    expect(rowsOf('agent 0')).toEqual(fixtures[0]!.expect.lines);
    expect(screen.queryByRole('log', { name: 'agent 1 output' })).toBeNull();
  });
});
