'use client';

// Connect / startup screen (matrix startup.probe, .loopback_permission,
// .recipe, .connect_browser, .version_handshake). Initial version by the
// foundation lane; the screens lane owns and extends it (launch-context
// picker, secondary attach). All copy comes from lib/connect/model.ts.
import type { ConnectController, ConnectState } from '@/lib/connect/controller';
import { OFFLINE_NOTE, connectView } from '@/lib/connect/model';
import { CommandBlock } from '../components/CommandBlock';

/** Plain text with `backtick` spans shown as code. Never parses HTML. */
function Inline({ text }: { text: string }) {
  return (
    <>
      {text.split('`').map((part, index) => (index % 2 === 1 ? <code key={index}>{part}</code> : part))}
    </>
  );
}

function formatExpiry(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? '' : date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

export function ConnectScreen({ state, controller }: { state: ConnectState; controller: ConnectController }) {
  const view = connectView(state);
  const has = (action: (typeof view.actions)[number]) => view.actions.includes(action);
  return (
    <main className="connect" aria-busy={view.busy}>
      <div className="connect-card" data-state={view.kind}>
        <h1>{view.title}</h1>
        {view.body.map(paragraph => (
          <p key={paragraph}>
            <Inline text={paragraph} />
          </p>
        ))}
        {view.recipe ? <CommandBlock commands={view.recipe} label="Setup commands" /> : null}
        {view.startCommand ? (
          <>
            <p>If the daemon is not running, also start it:</p>
            <CommandBlock commands={[view.startCommand]} label="Start command" />
          </>
        ) : null}
        {view.missing.length ? (
          <>
            <p>Missing in this daemon:</p>
            <ul className="missing">
              {view.missing.map(item => (
                <li key={item}>
                  <code>{item}</code>
                </li>
              ))}
            </ul>
          </>
        ) : null}
        {view.link ? (
          <div className="link-code">
            <p className="code" aria-label="Link code">
              {view.link.code}
            </p>
            <CommandBlock commands={[view.link.command]} label="Approval command" />
            <p className="hint">
              Waiting for approval… The code expires at {formatExpiry(view.link.expiresAt)}.
            </p>
          </div>
        ) : null}
        {view.statusHint ? (
          <p className="hint">
            <Inline text={view.statusHint} />
          </p>
        ) : null}
        <div className="actions">
          {has('allow') ? (
            <button type="button" onClick={() => void controller.allowLocalAccess()}>
              Allow local access
            </button>
          ) : null}
          {has('connect') ? (
            <button type="button" onClick={() => void controller.connectBrowser()}>
              Connect browser
            </button>
          ) : null}
          {has('retry_link') ? (
            <button type="button" onClick={() => void controller.connectBrowser()}>
              Request a new code
            </button>
          ) : null}
          {has('cancel_link') ? (
            <button type="button" className="secondary" onClick={() => controller.cancelLink()}>
              Cancel
            </button>
          ) : null}
          {has('recheck') ? (
            <button type="button" className={has('connect') ? 'secondary' : undefined} onClick={() => void controller.recheck()}>
              Recheck
            </button>
          ) : null}
        </div>
        <details className="about">
          <summary>About this app</summary>
          <p>
            awman runs entirely on this computer. This page talks only to the awman daemon at 127.0.0.1:9877; your sessions, commands and
            terminals never pass through the website. It works only with a daemon on the same machine.
          </p>
          <p>{OFFLINE_NOTE}</p>
        </details>
      </div>
    </main>
  );
}
