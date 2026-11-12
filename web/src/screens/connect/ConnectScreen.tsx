'use client';

import { useState } from 'react';
import type { ConnectController, ConnectState } from '@/lib/connect/controller';
import { OFFLINE_NOTE, RECOVERY_COPY, RECOVERY_TITLES, connectView } from '@/lib/connect/model';
import type { AuthEndState } from '@/lib/client/recovery';
import { DAEMON_STATUS } from '@/lib/connect/recipe';
import { CommandBlock } from '../components/CommandBlock';
import { EntryFrame } from '../components/EntryFrame';

/** Plain text with `backtick` spans shown as code. Never parses HTML. */
function Inline({ text }: { text: string }) {
  return <>{text.split('`').map((part, index) => index % 2 === 1 ? <code key={index}>{part}</code> : part)}</>;
}

function formatExpiry(iso: string, date = false): string {
  const value = new Date(iso);
  if (Number.isNaN(value.getTime())) return iso;
  return date ? value.toLocaleString() : value.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

/**
 * `ended` is why this document's authorization just ended. While the connect
 * flow re-probes it explains that precise state (never a blanket "revoked");
 * once the flow settles, its own view says what applies now.
 */
export function ConnectScreen({ state, controller, ended = null }: { state: ConnectState; controller: ConnectController; ended?: AuthEndState | null }) {
  const base = connectView(state);
  const view = ended && state.phase === 'probing' && !state.previous ? { ...base, title: RECOVERY_TITLES[ended], body: [RECOVERY_COPY[ended]] } : base;
  const [selected, setSelected] = useState('');
  const choice = view.choices.find(c => c.credentialId === selected) ?? view.choices[0];
  const has = (action: (typeof view.actions)[number]) => view.actions.includes(action);
  // Setup has one copy action first; saved authorization recovery never asks for
  // another approval merely because the daemon is unreachable.
  const diagnostic = view.notice === 'daemon_unreachable';
  const primary = view.link ? 'approval' : has('allow') ? 'allow'
    : view.choices.length ? has('choose_credential') ? 'choose_credential' : 'forget_credential'
    : has('retry_link') ? 'retry_link' : diagnostic ? 'status'
    : view.notice === 'daemon_stopped' ? 'start' : view.recipe ? 'setup'
    : has('connect') && view.notice !== 'reauth_unsupported' && view.notice !== 'listener_closed' && view.notice !== 'origin_removed' ? 'connect'
    : view.notice === 'rate_limited' || view.busy ? null : 'recheck';
  const buttonClass = (action: string) => primary === action ? undefined : 'secondary';
  const blocked = view.busy || view.notice === 'rate_limited';
  const guidance = <>
    {view.notice && view.body.slice(1).map(paragraph => <p key={paragraph}><Inline text={paragraph} /></p>)}
    {view.recipe && <CommandBlock commands={view.recipe} label="Setup commands" primary={primary === 'setup'} />}
    {view.startCommand && view.notice !== 'daemon_stopped' && <>
      <p>If the daemon is not running, also start it:</p>
      <CommandBlock commands={[view.startCommand]} label="Start command" />
    </>}
    {view.statusHint && <p className="hint"><Inline text={view.statusHint} /></p>}
  </>;
  const hasRecoveryGuidance = view.notice && (view.body.length > 1 || view.recipe || (view.startCommand && view.notice !== 'daemon_stopped') || view.statusHint);
  return <EntryFrame title={view.title} state={view.kind} busy={view.busy}>
    <div className="entry-explanation" role="status">
      {(view.notice ? view.body.slice(0, 1) : view.body).map(paragraph => <p key={paragraph}><Inline text={paragraph} /></p>)}
      {view.retryAfterSec !== null && <p className="hint">Automatic retry after {view.retryAfterSec} seconds.</p>}
    </div>
    {diagnostic && <CommandBlock commands={[DAEMON_STATUS]} label="Status command" primary />}
    {view.notice === 'daemon_stopped' && view.startCommand && <CommandBlock commands={[view.startCommand]} label="Start command" primary />}
    {(!view.notice || primary === 'setup') && guidance}
    {!!view.missing.length && <><p>Missing in this daemon:</p><ul className="missing">{view.missing.map(item => <li key={item}><code>{item}</code></li>)}</ul></>}
    {view.link && <div className="link-code">
      <p className="code" aria-label="Link code">{view.link.code}</p>
      <CommandBlock commands={[view.link.command]} label="Approval command" primary />
      <p className="hint" role="status">Waiting for approval… The code expires at {formatExpiry(view.link.expiresAt)}.</p>
    </div>}
    {!!view.choices.length && <fieldset className="entry-choices">
      <legend>Saved authorizations</legend>
      {view.choices.map((saved, index) => <label className="entry-choice" key={saved.credentialId}>
        <input type="radio" name="authorization" checked={choice?.credentialId === saved.credentialId} onChange={() => setSelected(saved.credentialId)} />
        <span><strong>{saved.listener} · Authorization {index + 1}</strong><span className="hint">{saved.expiresAt ? `Expires ${formatExpiry(saved.expiresAt, true)}` : 'Approval pending'}</span></span>
      </label>)}
    </fieldset>}
    <div className="actions">
      {has('choose_credential') && <button disabled={!choice || blocked} onClick={() => choice && void controller.chooseCredential(choice.credentialId)}>Reconnect with selected authorization</button>}
      {has('allow') && <button className={buttonClass('allow')} disabled={blocked} onClick={() => void controller.allowLocalAccess()}>Allow local access</button>}
      {has('connect') && <button className={buttonClass('connect')} disabled={blocked} onClick={() => void controller.connectBrowser()}>{view.notice === 'memory_only' ? 'Connect for this page' : 'Connect browser'}</button>}
      {has('retry_link') && <button className={buttonClass('retry_link')} disabled={blocked} onClick={() => void controller.connectBrowser()}>Request a new code</button>}
      {has('cancel_link') && <button className="secondary" onClick={() => controller.cancelLink()}>Cancel</button>}
      {has('cancel_recovery') && <button className="secondary" onClick={() => controller.cancelRecovery()}>Cancel reconnection</button>}
      {has('forget_credential') && !has('choose_credential') && <button disabled={!choice || blocked} onClick={() => choice && void controller.forgetCredential(choice.credentialId)}>Forget selected authorization</button>}
      {has('recheck') && <button className={buttonClass('recheck')} disabled={blocked} onClick={() => void controller.recheck()}>Recheck</button>}
    </div>
    {hasRecoveryGuidance && primary !== 'setup' && <details className="about"><summary>Connection details and setup</summary>{guidance}</details>}
    {has('forget_credential') && has('choose_credential') && <details className="about">
      <summary>Manage saved access</summary>
      <p>Forgetting removes the saved authorization from this browser. It does not revoke access already open in another tab.</p>
      <button className="secondary" disabled={!choice || blocked} onClick={() => choice && void controller.forgetCredential(choice.credentialId)}>Forget selected authorization</button>
    </details>}
    <details className="about">
      <summary>About this app</summary>
      <p>This page connects to the awman daemon on the host that served it; your sessions, commands and terminals never pass through the website. It works only with a daemon on the same machine.</p>
      <p>{OFFLINE_NOTE}</p>
    </details>
  </EntryFrame>;
}
