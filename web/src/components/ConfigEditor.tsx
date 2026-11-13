'use client';
import { useState } from 'react';
import type { ConfigShowRow, DialogRequestOf, DialogResponse } from '@/lib/client/types';
import { useScreenShortcuts as useShortcuts } from './ui';
import { VirtualList } from './ui';

export function ConfigEditor({ request, answer, disabled }: { request: DialogRequestOf<'config_show'>; answer: (r: DialogResponse) => void; disabled: boolean }) {
  const [selected, setSelected] = useState(request.selected);
  const [global, setGlobal] = useState(true);
  const [edit, setEdit] = useState<{ field: string; value: string } | null>(null);
  const row = request.rows[selected];
  function begin(target: ConfigShowRow | undefined = row, add = false) {
    if (disabled || !target || target.read_only || !(global ? target.global_writable : target.repo_writable)) return;
    const shape = target.shape;
    const prefix = typeof shape === 'object' ? ('map_header' in shape ? shape.map_header.child_prefix : shape.list_header.child_prefix) : null;
    if (add && !prefix) return;
    setEdit({ field: add ? prefix ?? target.field : target.field, value: add ? '' : (global ? target.global : target.repo) });
  }
  function save() { if (edit && !disabled) answer({ kind: 'config_edit', value: { ...edit, global } }); }
  useShortcuts(() => ({ context: 'dialog', opts: { dialog: 'config' } }), action => {
    if (disabled) return false;
    if (action === 'dialog.dismiss') { if (edit) setEdit(null); else answer({ kind: 'dismissed' }); return; }
    if (action === 'dialog.submit') { if (edit) save(); else begin(); return; }
    if (action === 'config.newEntry') { begin(row, true); return; }
    if (action.startsWith('nav.') && !edit) {
      if (action === 'nav.left' || action === 'nav.right') setGlobal(action === 'nav.left');
      else setSelected(i => Math.max(0, Math.min(request.rows.length - 1, i + (action === 'nav.up' ? -1 : action === 'nav.pageUp' ? -10 : action === 'nav.pageDown' ? 10 : 1))));
      return;
    }
    return false;
  });
  return <section className="config-editor" aria-label="Config table">
    {request.rejected && <p role="alert">{request.rejected.field}: {request.rejected.reason}</p>}
    <fieldset disabled={disabled}><legend>Configuration scope</legend><label><input type="radio" checked={global} onChange={() => setGlobal(true)} />Global</label><label><input type="radio" checked={!global} onChange={() => setGlobal(false)} />Repository</label></fieldset>
    <div className="config-head" aria-hidden="true">Field · Global · Repo · Effective</div>
    <VirtualList items={request.rows} activeIndex={selected} label="Configuration rows" render={(r, i) => <button className="config-row secondary" aria-pressed={selected === i} onClick={() => { setSelected(i); begin(r); }}><span title={r.field}>{r.field}</span><span title={r.global}>{r.global || '—'}</span><span title={r.repo}>{r.repo || '—'}</span><span title={r.effective}>{r.effective || '—'}{r.read_only ? ' (read only)' : ''}</span></button>} />
    <p className="hint">{row?.field}: {row?.value_hint}</p>
    {edit ? <fieldset disabled={disabled}><legend>Edit configuration</legend><label>Field<input aria-label="Field" value={edit.field} onChange={e => setEdit({ ...edit, field: e.target.value })} /></label><label>Value<input autoFocus aria-label="Value" value={edit.value} onChange={e => setEdit({ ...edit, value: e.target.value })} /></label><div className="actions"><button type="button" className="secondary" onClick={() => setEdit(null)}>Cancel</button><button type="button" className="secondary" onClick={() => setEdit({ ...edit, value: '' })}>Clear</button><button type="button" className="secondary danger" onClick={() => { if (!disabled) answer({ kind: 'config_edit', value: { field: edit.field, value: '', global } }); }}>Remove entry</button><button type="button" onClick={save}>Save</button></div></fieldset> : <div className="actions"><button disabled={disabled || !row || row.read_only || !(global ? row.global_writable : row.repo_writable)} onClick={() => begin()}>Edit</button><button className="secondary" disabled={disabled || !row || typeof row.shape !== 'object' || row.read_only || !(global ? row.global_writable : row.repo_writable)} onClick={() => begin(row, true)}>Add entry</button><button className="secondary" disabled={disabled} onClick={() => answer({ kind: 'dismissed' })}>Close</button></div>}
  </section>;
}
