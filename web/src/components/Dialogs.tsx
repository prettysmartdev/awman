'use client';
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import type { DaemonClient } from '@/lib/client/daemonClient';
import type { FrontendConnection } from '@/lib/client/frontendSocket';
import type { OpenDialog } from '@/lib/client/instanceStore';
import type { DialogRequest, DialogRequestOf, DialogResponse, KeyChoice, PromptKind, PromptView } from '@/lib/client/types';
import type { DialogScope, WebAction } from '@/lib/keys/keymap';
import { isTextInput } from '@/lib/keys/useShortcuts';
import { useScreenShortcuts as useShortcuts } from './ui';
import { countdownPresentation } from './presentation';
import { ConfigEditor } from './ConfigEditor';
import { TerminalPanel } from './TerminalPanel';
import { Modal, VirtualList, copyText, errorText } from './ui';

export function dialogScope(r: DialogRequest): DialogScope {
  switch (r.kind) {
    case 'yes_no': case 'yes_no_cancel': return r.kind;
    case 'text_input': case 'secret_input': return 'text';
    case 'multiline_input': return 'multiline';
    case 'list_picker': case 'kind_select': return 'list';
    case 'config_show': return 'config';
    case 'workflow_control_board': return 'control_board';
    case 'workflow_step_confirm': return 'step_confirm';
    case 'workflow_yolo_countdown': return 'yolo';
    case 'workflow_cancel_confirm': return 'yes_no';
    case 'quit_confirm': case 'close_tab_confirm': case 'key_setup_notice': case 'mount_scope': return r.kind;
    default: return 'other';
  }
}
export function usePrompt(client: DaemonClient, kind: PromptKind | null, params?: Record<string, string>) {
  const [prompt, setPrompt] = useState<PromptView | null>(null);
  const [error, setError] = useState('');
  const key = JSON.stringify(params ?? {});
  useEffect(() => {
    let live = true; setPrompt(null); setError('');
    if (kind) void client.prompt(kind, JSON.parse(key) as Record<string, string>).then(p => { if (live) setPrompt(p); }).catch(e => { if (live) setError(errorText(e)); });
    return () => { live = false; };
  }, [client, kind, key]);
  return { prompt, error };
}
export function PromptButtons({ prompt, choose, disabled = false }: { prompt: PromptView; choose: (value: string) => void; disabled?: boolean }) {
  return <div className="actions">{prompt.choices.map(c => <button key={c.key} disabled={disabled} onClick={() => choose(c.value)}>{c.label}</button>)}</div>;
}

export function ReviewBody({ request, onEnd }: { request: DialogRequestOf<'review'>; onEnd: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  const check = useCallback(() => { const node = ref.current; if (node && node.scrollHeight - node.scrollTop <= node.clientHeight + 2) onEnd(); }, [onEnd]);
  useLayoutEffect(() => { check(); const node = ref.current; const observer = typeof ResizeObserver === 'function' ? new ResizeObserver(check) : null; if (node) observer?.observe(node); return () => observer?.disconnect(); }, [check]);
  return <div className="review-body" tabIndex={0} ref={ref} onScroll={check} aria-label="Review body">{request.body.kind === 'prerequisites' ? <>
    {request.body.value.checks.map((c, i) => <article key={i}><h3>{c.prerequisite.type} · {c.status}</h3><p>{c.explanation}</p><ul>{c.needed_by.map((s, j) => <li key={j}>{s.description}</li>)}</ul><pre>{JSON.stringify(c.remediations, null, 2)}</pre></article>)}
    {request.body.value.advisories.map((a, i) => <pre key={i}>{JSON.stringify(a, null, 2)}</pre>)}{request.body.value.notes.map((n, i) => <p key={i}>{n}</p>)}
  </> : <pre>{request.body.value}</pre>}</div>;
}

export function DialogRenderer({ dialog, client, connection, readOnly = false, onLocalDismiss }: { dialog: OpenDialog; client: DaemonClient; connection: FrontendConnection; readOnly?: boolean; onLocalDismiss?: () => void }) {
  const r = dialog.request;
  // Secret values live only in the uncontrolled password element. No React
  // state, retained draft, storage or payload logging ever receives them.
  const input = useRef<HTMLInputElement & HTMLTextAreaElement>(null);
  const [selected, setSelected] = useState(0);
  const [sent, setSent] = useState(false);
  const [error, setError] = useState('');
  const [reviewed, setReviewed] = useState(false);
  const markReviewed = useCallback(() => setReviewed(true), []);
  const promptKind = r.kind === 'quit_confirm' || r.kind === 'close_tab_confirm' || r.kind === 'workflow_cancel_confirm' ? r.kind : null;
  const { prompt, error: promptError } = usePrompt(client, promptKind);
  const disabled = readOnly || sent;
  const answer = useCallback((response: DialogResponse) => {
    // A countdown is a non-answering overlay (the engine publishes it as
    // Yolo/SlotYolo state): its controls send CancelCountdown/WorkflowRequest,
    // never a DialogResponse for an id the engine did not raise.
    if (readOnly || sent || r.kind === 'workflow_yolo_countdown') return;
    try {
      connection.send({ type: 'dialog_response', dialog_id: dialog.dialogId, response });
      if (input.current) input.current.value = '';
      setSent(true); setError('');
    } catch { if (input.current && r.kind === 'secret_input') input.current.value = ''; setError('Response was not sent. Reconnect and check the log.'); }
  }, [connection, dialog.dialogId, r.kind, readOnly, sent]);
  const autoAnswer = useRef(false);
  const mounted = useCallback(() => { if (autoAnswer.current || readOnly) return; autoAnswer.current = true; answer({ kind: 'yes' }); }, [answer, readOnly]);
  useEffect(() => {
    if (r.kind === 'terminal_restore' && !autoAnswer.current && !readOnly) { autoAnswer.current = true; answer({ kind: 'dismissed' }); }
  }, [r.kind, answer, readOnly]);
  function dismiss() {
    if (r.kind === 'loading') return;
    if (r.kind === 'key_setup_notice') { onLocalDismiss?.(); return; }
    if (r.kind === 'workflow_yolo_countdown') { countdown(); return; }
    answer({ kind: 'dismissed' });
  }
  function countdown() { if (!disabled && r.kind === 'workflow_yolo_countdown') { try { connection.send({ type: 'cancel_countdown', tab_id: dialog.tabId, step_name: r.step_name }); } catch (e) { setError(errorText(e)); } } }
  function board() {
    if (disabled) return;
    if (r.kind === 'workflow_step_confirm') { answer({ kind: 'char', value: 'W' }); return; }
    if (r.kind === 'workflow_yolo_countdown') { try { connection.send({ type: 'workflow_request', tab_id: dialog.tabId, request: { request: 'open_control_board', step_name: r.step_name } }); } catch (e) { setError(errorText(e)); } }
  }
  const char = (value: string) => answer({ kind: 'char', value });
  const boardChoices: [WebAction, string, string, boolean, string | null][] = r.kind === 'workflow_control_board' ? [
    ['wfc.nextNewContainer', '→ Next: new container', '>', r.can_launch_next, null],
    ['wfc.nextSameContainer', '↓ Next: same container', 'v', r.can_continue_current, r.continue_unavailable_reason],
    ['wfc.restart', '↑ Restart', '^', r.can_restart && !r.restart_unavailable_reason, r.restart_unavailable_reason],
    ['wfc.back', '← Back', '<', r.can_go_back, r.cancel_to_previous_unavailable_reason],
    ['wfc.finish', 'Finish', 'f', r.can_finish, r.finish_workflow_unavailable_reason],
    ['wfc.retryFailedPeer', 'Retry', 'r', r.retry_failed_step !== null, r.retry_failed_step],
    ['wfc.pause', 'Pause', 'p', r.can_dismiss, null], ['wfc.abort', 'Abort', 'a', true, null],
  ] : [];
  const offered: KeyChoice[] = r.kind === 'custom' || r.kind === 'review' ? r.keys : r.kind === 'agent_setup' ? [['y', 'Yes'], ['n', 'No'], ...(r.has_fallback && r.fallback_name ? [['f', `Fallback to ${r.fallback_name}`] as KeyChoice] : [])] : r.kind === 'agent_auth' ? [['y', 'Accept'], ['n', 'Decline'], ['o', 'Decline once']] : [];
  function submit() {
    if (r.kind === 'text_input' || r.kind === 'multiline_input' || r.kind === 'secret_input') answer({ kind: r.kind === 'secret_input' ? 'secret' : 'text', value: input.current?.value ?? '' });
    if (r.kind === 'list_picker' || r.kind === 'kind_select') { const length = r.kind === 'list_picker' ? r.items.length : r.options.length; if (length) answer({ kind: 'index', value: selected }); }
    if (r.kind === 'workflow_step_confirm') char('>');
    if (r.kind === 'custom' && r.keys.length === 1 && r.keys[0]) char(r.keys[0][0]);
    if (r.kind === 'key_setup_notice') onLocalDismiss?.();
  }
  useShortcuts(() => ({ context: 'dialog', opts: { dialog: dialogScope(r) } }), action => {
    if (r.kind === 'config_show' && (action.startsWith('nav.') || action === 'config.newEntry' || action === 'dialog.submit' || action === 'dialog.dismiss')) return false;
    if (action === 'dialog.dismiss') { dismiss(); return; }
    if (action === 'dialog.submit') { submit(); return; }
    if (action === 'dialog.yes') { if (r.kind === 'workflow_cancel_confirm') char('a'); else answer({ kind: 'yes' }); return; }
    if (action === 'dialog.no') { answer(r.kind === 'workflow_cancel_confirm' ? { kind: 'dismissed' } : { kind: 'no' }); return; }
    if (action === 'dialog.cancel') { answer({ kind: 'cancel' }); return; }
    if (action === 'dialog.quit') { answer({ kind: 'yes' }); return; }
    if (action === 'dialog.closeTab') { answer({ kind: 'no' }); return; }
    if (action === 'mountScope.gitRoot') { char('r'); return; }
    if (action === 'mountScope.cwd') { char('c'); return; }
    if (action === 'workflow.control') { board(); return; }
    if (action === 'keySetup.copyCommand' && r.kind === 'key_setup_notice') { void copyText(r.copy_key).catch(() => setError('Copy failed.')); return; }
    if (action === 'keySetup.copyAlternate' && r.kind === 'key_setup_notice') { void copyText(r.copy_zshrc_snippet).catch(() => setError('Copy failed.')); return; }
    const choice = boardChoices.find(([a]) => a === action); if (choice) { if (choice[3]) char(choice[2]); return; }
    if ((action === 'nav.up' || action === 'nav.down') && (r.kind === 'list_picker' || r.kind === 'kind_select')) { const length = r.kind === 'list_picker' ? r.items.length : r.options.length; setSelected(i => Math.max(0, Math.min(length - 1, i + (action === 'nav.up' ? -1 : 1)))); return; }
    return false;
  });
  let title = 'title' in r ? r.title : r.kind.replaceAll('_', ' ');
  let content: ReactNode;
  const cancel = <button type="button" className="secondary" disabled={disabled} onClick={dismiss}>Cancel</button>;
  switch (r.kind) {
    case 'yes_no': case 'yes_no_cancel': content = <><p className="prose">{r.body}</p><div className="actions"><button disabled={disabled} onClick={() => answer({ kind: 'yes' })}>Yes</button><button disabled={disabled} onClick={() => answer({ kind: 'no' })}>No</button>{r.kind === 'yes_no_cancel' && <button disabled={disabled} onClick={() => answer({ kind: 'cancel' })}>Cancel</button>}</div></>; break;
    case 'text_input': case 'multiline_input': case 'secret_input': content = <form onSubmit={e => { e.preventDefault(); submit(); }}><label>{r.prompt}{r.kind === 'multiline_input' ? <textarea ref={input} defaultValue={r.default_text ?? ''} disabled={disabled} aria-label={r.prompt} /> : <input ref={input} type={r.kind === 'secret_input' ? 'password' : 'text'} autoComplete="off" defaultValue={r.kind === 'text_input' ? r.default_text ?? '' : ''} disabled={disabled} aria-label={r.prompt} />}</label><div className="actions"><button disabled={disabled}>{r.kind === 'multiline_input' ? 'Submit' : 'OK'}</button>{cancel}</div></form>; break;
    case 'list_picker': case 'kind_select': { const items = r.kind === 'list_picker' ? r.items : r.options.map(o => o[1]); content = <><VirtualList label="Choices" activeIndex={selected} items={items} render={(item, i) => <button className="secondary choice" aria-pressed={selected === i} onClick={() => setSelected(i)}>{item}</button>} /><div className="actions"><button disabled={disabled || !items.length} onClick={submit}>Select</button>{cancel}</div></>; break; }
    case 'mount_scope': title = 'Mount Scope'; content = <><p>Git root: {r.git_root}</p><p>CWD: {r.cwd}</p><div className="actions"><button disabled={disabled} onClick={() => char('r')}>Git root</button><button disabled={disabled} onClick={() => char('c')}>Current directory</button>{cancel}</div></>; break;
    case 'agent_setup': title = r.image_only ? `Build ${r.agent_name} image?` : `Set up ${r.agent_name}?`; content = <div className="actions">{offered.map(([key, label]) => <button key={key} disabled={disabled} onClick={() => char(key)}>{label}</button>)}{cancel}</div>; break;
    case 'agent_auth': title = 'Agent credentials?'; content = <><p>Agent: {r.agent_name}</p><p>Env vars to inject:</p><ul>{r.env_vars.map(v => <li key={v}>{v}</li>)}</ul><div className="actions">{offered.map(([key, label]) => <button key={key} disabled={disabled} onClick={() => char(key)}>{label}</button>)}{cancel}</div></>; break;
    case 'custom': content = <><p className="prose">{r.body}</p><div className="actions">{r.keys.map(([key, label]) => <button key={key} disabled={disabled} onClick={() => char(key)}>{label}</button>)}{cancel}</div></>; break;
    case 'review': content = <><ReviewBody request={r} onEnd={markReviewed} /><p>{r.note}</p><div className="actions">{r.keys.map(([key, label]) => <button key={key} disabled={disabled || (r.gated_keys.includes(key) && !reviewed)} onClick={() => char(key)}>{label}</button>)}<button className="secondary" disabled={disabled} onClick={dismiss}>{r.dismiss_label ?? 'Cancel'}</button></div></>; break;
    case 'config_show': title = 'Configuration'; content = <ConfigEditor request={r} answer={answer} disabled={disabled} />; break;
    case 'workflow_control_board': title = r.failure_lines.length ? 'Workflow Control — step failed' : r.can_dismiss ? 'Workflow Control (step running)' : 'Workflow Control'; content = <><p>Step: {r.step_name}</p><p>Focused step: {r.focused_step_name} · {r.parallel_peers_running}/{r.parallel_peer_count} peers running{r.in_parallel_group ? ' · parallel group' : ''}</p>{r.failure_lines.map((line, i) => <p role="alert" key={i}>{line}</p>)}{r.launch_next_label && <p>{r.launch_next_label}</p>}<div className="board-actions">{boardChoices.map(([action, label, key, available, reason]) => <div key={action}><button disabled={disabled || !available} onClick={() => char(key)}>{label}</button>{reason && <p className="hint">{reason}</p>}</div>)}</div><button className="secondary" disabled={disabled} onClick={dismiss}>{r.can_dismiss ? 'Close' : 'Pause'}</button></>; break;
    case 'workflow_step_confirm': title = 'Step Complete'; content = <><p>Step '{r.completed_step}' done. Advance to '{r.next_step}'?</p><div className="actions"><button disabled={disabled} onClick={() => char('>')}>Next</button><button disabled={disabled} onClick={dismiss}>Pause</button><button disabled={disabled} onClick={board}>Workflow</button></div></>; break;
    case 'workflow_yolo_countdown': title = countdownPresentation(r.countdown_kind, r.remaining_secs).title; content = <><p>Step: {r.step_name}</p><p>{countdownPresentation(r.countdown_kind, r.remaining_secs).label}</p><div className="actions"><button disabled={disabled} onClick={countdown}>Cancel countdown</button><button disabled={disabled} onClick={board}>Workflow</button></div></>; break;
    case 'quit_confirm': case 'close_tab_confirm': case 'workflow_cancel_confirm': title = prompt?.title ?? 'Loading prompt…'; content = <>{promptError && <p role="alert">{promptError}</p>}{prompt && <><p className="prose">{prompt.body}</p><PromptButtons prompt={prompt} disabled={disabled} choose={value => { if (r.kind === 'workflow_cancel_confirm') answer(value === 'abort' || value === 'yes' ? { kind: 'char', value: 'a' } : { kind: 'dismissed' }); else answer({ kind: value === 'quit' || value === 'yes' ? 'yes' : value === 'close_tab' || value === 'no' ? 'no' : 'cancel' }); }} /></>}</>; break;
    case 'loading': content = <p role="status" aria-busy="true">Loading…</p>; break;
    case 'key_setup_notice': content = <><p className="prose">{r.body}</p><div className="actions"><button onClick={() => void copyText(r.copy_key).catch(() => setError('Copy failed.'))}>Copy key</button><button onClick={() => void copyText(r.copy_zshrc_snippet).catch(() => setError('Copy failed.'))}>Copy snippet</button><button onClick={onLocalDismiss}>Dismiss</button></div></>; break;
    case 'terminal_release': content = r.terminal_id ? <TerminalPanel client={client} terminalId={r.terminal_id} title={r.title} readOnly={readOnly} onMounted={mounted} /> : <><p>Waiting for the engine terminal.</p><button disabled={disabled} onClick={() => answer({ kind: 'yes' })}>Continue</button></>; break;
    case 'terminal_restore': title = 'Restoring workspace'; content = <p role="status">Restoring workspace…</p>; break;
  }
  return <Modal title={title}><div onKeyDown={event => {
    if (event.ctrlKey || event.altKey || event.metaKey || isTextInput(event.target) || disabled) return;
    const key = event.key.toLowerCase(); const choice = offered.find(([k]) => k === key);
    if (choice && (r.kind !== 'review' || !r.gated_keys.includes(key) || reviewed)) { char(choice[0]); event.preventDefault(); }
  }}>{readOnly && <p>Read only: answers belong to the primary frontend.</p>}{error && <p role="alert">{error}</p>}{sent && <p role="status">Waiting for the engine to confirm the answer…</p>}{content}</div></Modal>;
}
