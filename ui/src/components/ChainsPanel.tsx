// Settings → Models → Fallback chains (admin only).
//
// A chain is an ORDERED list of rungs. Position 0 is tried first; each later
// rung is the fallback for the one above it. Exactly one chain per (role, scope)
// can be active at a time — activating one deactivates its siblings server-side.
import { useCallback, useEffect, useState } from 'react';
import {
  addChainRung,
  createChain,
  deleteChain,
  deleteChainRung,
  fetchChains,
  reorderChainRungs,
  setChainActive,
} from '../api';
import { useToast } from '../toast';
import { MODEL_ROLES, type ModelChainWithRungs, type ModelRole, type ModelRow } from '../types';
import { EmptyState, Skeleton } from './Skeleton';
import { StatusChip } from './StatusChip';

interface ChainsPanelProps {
  /** Every model across every provider — used to populate the "add rung" picker. */
  models: ModelRow[];
  /** provider_id → provider name, so the picker can show where a model lives. */
  providerNames: Record<string, string>;
  /** Live vector-index width; an embed rung of any other width is refused on save. */
  indexDim: number;
}

function modelLabel(m: ModelRow, providerName: string | undefined): string {
  const base = m.label && m.label !== m.model_id ? `${m.label} (${m.model_id})` : m.model_id;
  return providerName ? `${providerName} · ${base}` : base;
}

export function ChainsPanel({ models, providerNames, indexDim }: ChainsPanelProps) {
  const toast = useToast();
  const [chains, setChains] = useState<ModelChainWithRungs[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  /** Long, explanatory API rejections (the embed-dim one especially) shown verbatim. */
  const [actionError, setActionError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [newName, setNewName] = useState('');
  const [newRole, setNewRole] = useState<ModelRole>('chat');
  const [addModelId, setAddModelId] = useState<Record<string, string>>({});

  const load = useCallback(async () => {
    setLoadError(null);
    try {
      setChains(await fetchChains());
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : 'Failed to load chains');
      setChains([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function run(action: () => Promise<unknown>, okMessage: string) {
    if (busy) return;
    setBusy(true);
    setActionError(null);
    try {
      await action();
      toast.push(okMessage, 'success');
      await load();
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'Request failed';
      setActionError(msg);
      toast.push('The chain was not saved — see the message above the chains.', 'error');
    } finally {
      setBusy(false);
    }
  }

  function createNewChain() {
    if (!newName.trim()) {
      setActionError('Give the chain a name first.');
      return;
    }
    void run(async () => {
      await createChain({ name: newName.trim(), role: newRole });
      setNewName('');
    }, 'Chain created — add rungs to it, then activate it.');
  }

  function moveRung(chain: ModelChainWithRungs, index: number, dir: -1 | 1) {
    const ids = chain.rungs.map((r) => r.id);
    const target = index + dir;
    if (target < 0 || target >= ids.length) return;
    [ids[index], ids[target]] = [ids[target], ids[index]];
    void run(() => reorderChainRungs(chain.id, ids), 'Fallback order updated');
  }

  return (
    <div className="card">
      <div className="card-title">Fallback chains</div>
      <p className="muted" style={{ marginTop: 0, fontSize: 13 }}>
        A chain is tried <b>top to bottom</b>: position <b>0</b> is called first, and each rung below it
        is the fallback for the one above. One chain per role can be active at a time.
      </p>

      <div className="info-note" style={{ marginBottom: 14 }}>
        <span className="info-note-icon" aria-hidden="true">
          ℹ
        </span>
        <span className="info-note-text">
          <b>Chat</b> falls back to the environment (<code className="mono">DARIO_MODEL_PRIMARY</code> /{' '}
          <code className="mono">DARIO_MODEL_FALLBACK</code>) when no chat chain is active — leaving this
          empty breaks nothing. <b>Embed</b> is different: it has no such env fallback here, so an embed
          chain either uses the models below or the separate <code className="mono">EMBED_PROVIDER</code>{' '}
          / <code className="mono">EMBED_CHAIN</code> configuration. Every embed rung must return{' '}
          <b>{indexDim}-d</b> vectors — the live index is that width and mixing widths corrupts it
          silently.
        </span>
      </div>

      {loadError && <div className="error-box">{loadError}</div>}
      {actionError && (
        <div className="error-box" style={{ marginBottom: 14 }}>
          {actionError}
        </div>
      )}

      <div className="rule-form" style={{ marginBottom: 14 }}>
        <input
          type="text"
          className="input"
          placeholder="New chain name — e.g. “chat primary”"
          value={newName}
          onChange={(e) => setNewName(e.target.value)}
          aria-label="New chain name"
        />
        <select
          className="input"
          value={newRole}
          onChange={(e) => setNewRole(e.target.value as ModelRole)}
          aria-label="New chain role"
          data-hint="Which job the chain serves: chat (link rewriting / insertion modules) or embed (vector search)."
        >
          {MODEL_ROLES.map((r) => (
            <option key={r} value={r}>
              {r}
            </option>
          ))}
        </select>
        <button
          className="btn btn-primary"
          disabled={busy}
          onClick={createNewChain}
          data-hint="Creates an empty, inactive chain. Add rungs to it and then activate it."
        >
          Add chain
        </button>
      </div>

      {chains === null ? (
        <Skeleton rows={5} />
      ) : chains.length === 0 ? (
        <EmptyState
          title="No chains configured"
          hint="Chat keeps working from the environment config until you create one."
        />
      ) : (
        <div className="chain-list">
          {chains.map((c) => {
            const candidates = models.filter((m) => m.role === c.role);
            const picked = addModelId[c.id] ?? '';
            return (
              <div key={c.id} className={`chain-card${c.active ? ' active' : ''}`}>
                <div className="chain-head">
                  <span className="chain-name">{c.name}</span>
                  <span className="chip chip-mode">{c.role}</span>
                  <span className="chip chip-mode">{c.site_id ? 'site' : 'global'}</span>
                  <StatusChip status={c.active ? 'applied' : 'suggested'} />
                  <div className="spacer" />
                  <button
                    className={`btn btn-sm${c.active ? '' : ' btn-primary'}`}
                    disabled={busy}
                    onClick={() =>
                      void run(
                        () => setChainActive(c.id, !c.active),
                        c.active ? 'Chain deactivated' : 'Chain activated',
                      )
                    }
                    data-hint={
                      c.active
                        ? 'Stop using this chain. With no active chain the role falls back to the environment configuration.'
                        : 'Make this the chain used for its role. Any other active chain for the same role is switched off.'
                    }
                  >
                    {c.active ? 'Deactivate' : 'Activate'}
                  </button>
                  <button
                    className="btn btn-sm btn-danger"
                    disabled={busy}
                    onClick={() => void run(() => deleteChain(c.id), 'Chain deleted')}
                    data-hint="Delete this chain and all of its rungs. The models themselves are kept."
                  >
                    Delete
                  </button>
                </div>

                {c.rungs.length === 0 ? (
                  <div className="muted" style={{ padding: '6px 0' }}>
                    No rungs yet — add one below. An empty chain serves nothing.
                  </div>
                ) : (
                  <ol className="rung-list">
                    {c.rungs.map((r, i) => (
                      <li key={r.id} className={`rung${r.enabled ? '' : ' rung-off'}`}>
                        <span className="rung-pos" title={i === 0 ? 'Tried first' : `Fallback #${i}`}>
                          {i}
                        </span>
                        <span className="rung-name">
                          <b>{r.provider_name}</b> · <span className="mono">{r.wire_model_id}</span>
                        </span>
                        {r.model_role === 'embed' && (
                          <span className={`chip chip-${r.dim === indexDim ? 'done' : 'failed'}`}>
                            {r.dim === null ? 'dim unknown' : `${r.dim}-d`}
                          </span>
                        )}
                        {i === 0 && <span className="chip chip-approved">tried first</span>}
                        <div className="spacer" />
                        <button
                          className="btn btn-sm"
                          disabled={busy || i === 0}
                          onClick={() => moveRung(c, i, -1)}
                          aria-label={`Move ${r.wire_model_id} earlier in the fallback order`}
                          data-hint="Move this model earlier — it will be tried before the one above it."
                        >
                          ↑
                        </button>
                        <button
                          className="btn btn-sm"
                          disabled={busy || i === c.rungs.length - 1}
                          onClick={() => moveRung(c, i, 1)}
                          aria-label={`Move ${r.wire_model_id} later in the fallback order`}
                          data-hint="Move this model later — it becomes the fallback for the one below it."
                        >
                          ↓
                        </button>
                        <button
                          className="btn btn-sm btn-danger"
                          disabled={busy}
                          onClick={() => void run(() => deleteChainRung(r.id), 'Rung removed')}
                          aria-label={`Remove ${r.wire_model_id} from this chain`}
                          data-hint="Remove this model from the chain. The model itself is kept."
                        >
                          ✕
                        </button>
                      </li>
                    ))}
                  </ol>
                )}

                <div className="rule-form">
                  <select
                    className="input"
                    value={picked}
                    onChange={(e) => setAddModelId({ ...addModelId, [c.id]: e.target.value })}
                    aria-label={`Add a model to ${c.name}`}
                    data-hint={`Only ${c.role} models can be rungs of a ${c.role} chain.`}
                  >
                    <option value="">Add a {c.role} model…</option>
                    {candidates.map((m) => (
                      <option key={m.id} value={m.id}>
                        {modelLabel(m, providerNames[m.provider_id])}
                        {m.role === 'embed' ? ` — ${m.dim === null ? 'dim unknown' : `${m.dim}-d`}` : ''}
                      </option>
                    ))}
                  </select>
                  <button
                    className="btn"
                    disabled={busy || picked === ''}
                    onClick={() =>
                      void run(async () => {
                        await addChainRung(c.id, { model_row_id: picked });
                        setAddModelId({ ...addModelId, [c.id]: '' });
                      }, 'Rung added to the bottom of the chain')
                    }
                    data-hint="Appends the selected model as the last fallback in this chain."
                  >
                    Add rung
                  </button>
                  {candidates.length === 0 && (
                    <span className="faint" style={{ alignSelf: 'center', fontSize: 12.5 }}>
                      No {c.role} models exist yet — add one under a provider first.
                    </span>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
