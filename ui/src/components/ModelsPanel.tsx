// Settings → Models (ADMIN ONLY).
//
// Mirrors the shape of a model-control portal: add a provider → add models under
// it → order those models into a named fallback chain per role → probe health.
// The API never returns an API key: it returns a mask plus `has_api_key`, so a
// blank key field on edit means "leave the stored key alone".
import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  ApiError,
  createModel,
  createProvider,
  deleteModel,
  deleteProvider,
  fetchProvidersHealth,
  patchModel,
  patchProvider,
  probeModel,
  probeProvider,
  type ModelInput,
  type ProviderInput,
} from '../api';
import { fmtDate } from '../format';
import { useToast } from '../toast';
import {
  MODEL_ROLES,
  PROVIDER_KINDS,
  type ModelProvider,
  type ModelRole,
  type ModelRow,
  type ProbeResult,
  type ProviderHealth,
  type ProviderKind,
} from '../types';
import { ChainsPanel } from './ChainsPanel';
import { Modal } from './Modal';
import { EmptyState, Skeleton } from './Skeleton';
import { StatusChip } from './StatusChip';

const DEFAULT_INDEX_DIM = 384;

/** Kind-specific base-URL guidance — the anthropic one prevents a confusing 404 at probe time. */
const BASE_URL_HINTS: Record<ProviderKind, string> = {
  'openai-compatible':
    'The OpenAI-compatible root INCLUDING the version segment — e.g. https://api.openai.com/v1. The adapter appends /chat/completions and /embeddings.',
  anthropic:
    'The host ROOT only — https://api.anthropic.com, NOT .../v1. The adapter appends /v1/messages itself; adding /v1 yourself produces a 404 at probe time.',
  openrouter: 'The OpenRouter API root including the version — https://openrouter.ai/api/v1.',
  ollama: 'The Ollama host root — e.g. http://localhost:11434. The adapter appends /api/chat and /api/embeddings.',
  custom:
    'Whatever root your gateway expects. Requests are shaped like OpenAI: <base>/chat/completions and <base>/embeddings.',
};

const BASE_URL_PLACEHOLDER: Record<ProviderKind, string> = {
  'openai-compatible': 'https://api.openai.com/v1',
  anthropic: 'https://api.anthropic.com',
  openrouter: 'https://openrouter.ai/api/v1',
  ollama: 'http://localhost:11434',
  custom: 'https://gateway.internal/v1',
};

function probeChipStatus(ok: boolean | null): string {
  if (ok === null) return 'suggested';
  return ok ? 'done' : 'failed';
}

function ProbeChip({ ok, at, ms }: { ok: boolean | null; at: string | null; ms: number | null }) {
  const label = ok === null ? 'never probed' : ok ? `ok${ms !== null ? ` · ${ms}ms` : ''}` : 'failed';
  return (
    <span
      className={`chip chip-${probeChipStatus(ok)}`}
      title={at ? `Last probed ${fmtDate(at)}` : 'This target has never been probed'}
    >
      {label}
    </span>
  );
}

// ---------------------------------------------------------------------------
// Provider form
// ---------------------------------------------------------------------------

interface ProviderFormState {
  name: string;
  label: string;
  kind: ProviderKind;
  base_url: string;
  api_key: string;
  enabled: boolean;
}

const EMPTY_PROVIDER: ProviderFormState = {
  name: '',
  label: '',
  kind: 'openai-compatible',
  base_url: '',
  api_key: '',
  enabled: true,
};

function ProviderForm({
  initial,
  editing,
  busy,
  onCancel,
  onSubmit,
}: {
  initial: ProviderFormState;
  editing: ModelProvider | null;
  busy: boolean;
  onCancel: () => void;
  onSubmit: (v: ProviderFormState) => void;
}) {
  const [form, setForm] = useState<ProviderFormState>(initial);
  const [error, setError] = useState<string | null>(null);

  function submit() {
    if (!form.name.trim()) {
      setError('Give the provider a short name (it is the identifier used in chains).');
      return;
    }
    if (!/^https?:\/\//i.test(form.base_url.trim())) {
      setError('Base URL must start with http:// or https://.');
      return;
    }
    setError(null);
    onSubmit(form);
  }

  return (
    <Modal
      title={editing ? `Edit provider: ${editing.name}` : 'Add provider'}
      onClose={onCancel}
      footer={
        <>
          <button className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={busy} onClick={submit}>
            {busy ? 'Saving…' : editing ? 'Save provider' : 'Add provider'}
          </button>
        </>
      }
    >
      <div className="form-grid">
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          Name
          <input
            type="text"
            value={form.name}
            onChange={(e) => setForm({ ...form, name: e.target.value })}
            placeholder="openrouter"
            autoFocus
          />
          <span className="faint" style={{ fontSize: 12 }}>
            Short identifier, shown next to every model and rung.
          </span>
        </label>
        <label className="field">
          Label
          <input
            type="text"
            value={form.label}
            onChange={(e) => setForm({ ...form, label: e.target.value })}
            placeholder="OpenRouter (prod key)"
          />
        </label>
        <label className="field">
          Kind
          <select value={form.kind} onChange={(e) => setForm({ ...form, kind: e.target.value as ProviderKind })}>
            {PROVIDER_KINDS.map((k) => (
              <option key={k} value={k}>
                {k}
              </option>
            ))}
          </select>
        </label>
        <label className="field">
          Base URL
          <input
            type="text"
            value={form.base_url}
            onChange={(e) => setForm({ ...form, base_url: e.target.value })}
            placeholder={BASE_URL_PLACEHOLDER[form.kind]}
            spellCheck={false}
            data-hint={BASE_URL_HINTS[form.kind]}
          />
          <span className="faint" style={{ fontSize: 12 }}>
            {BASE_URL_HINTS[form.kind]}
          </span>
        </label>
        <label className="field">
          API key
          <input
            type="password"
            value={form.api_key}
            onChange={(e) => setForm({ ...form, api_key: e.target.value })}
            placeholder={editing?.has_api_key ? editing.api_key_masked : 'paste the key'}
            autoComplete="new-password"
            spellCheck={false}
          />
          <span className="faint" style={{ fontSize: 12 }}>
            {editing
              ? 'The stored key is never sent back to the browser. Leave this blank to KEEP the existing key — typing here replaces it.'
              : 'Optional. Leave blank for providers that need no auth (a local Ollama, for example).'}
          </span>
        </label>
        <label className="field">
          <span className="dup-toggle">
            <input
              type="checkbox"
              checked={form.enabled}
              onChange={(e) => setForm({ ...form, enabled: e.target.checked })}
            />
            Enabled
          </span>
        </label>
      </div>
    </Modal>
  );
}

// ---------------------------------------------------------------------------
// Model form
// ---------------------------------------------------------------------------

interface ModelFormState {
  model_id: string;
  label: string;
  role: ModelRole;
  context_tokens: string;
  dim: string;
  price_in: string;
  price_out: string;
  enabled: boolean;
  notes: string;
}

const EMPTY_MODEL: ModelFormState = {
  model_id: '',
  label: '',
  role: 'chat',
  context_tokens: '',
  dim: '',
  price_in: '',
  price_out: '',
  enabled: true,
  notes: '',
};

function numOrUndef(raw: string): number | undefined {
  const t = raw.trim();
  if (t === '') return undefined;
  const n = Number(t);
  return Number.isFinite(n) ? n : undefined;
}

function ModelForm({
  initial,
  editing,
  providerName,
  indexDim,
  busy,
  onCancel,
  onSubmit,
}: {
  initial: ModelFormState;
  editing: ModelRow | null;
  providerName: string;
  indexDim: number;
  busy: boolean;
  onCancel: () => void;
  onSubmit: (v: ModelFormState) => void;
}) {
  const [form, setForm] = useState<ModelFormState>(initial);
  const [error, setError] = useState<string | null>(null);

  const dimValue = numOrUndef(form.dim);
  const dimMismatch = form.role === 'embed' && dimValue !== undefined && dimValue !== indexDim;

  function submit() {
    if (!form.model_id.trim()) {
      setError('Enter the model id exactly as the provider expects it on the wire.');
      return;
    }
    if (form.role === 'embed' && dimValue === undefined) {
      setError(
        `Embed models need a vector width. The live index is ${indexDim}-d and a chain refuses any embed rung whose dim is unknown or different — probe the model to learn its width if you are unsure.`,
      );
      return;
    }
    setError(null);
    onSubmit(form);
  }

  return (
    <Modal
      title={editing ? `Edit model: ${editing.model_id}` : `Add model to ${providerName}`}
      onClose={onCancel}
      footer={
        <>
          <button className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={busy} onClick={submit}>
            {busy ? 'Saving…' : editing ? 'Save model' : 'Add model'}
          </button>
        </>
      }
    >
      <div className="form-grid">
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          Model id
          <input
            type="text"
            value={form.model_id}
            onChange={(e) => setForm({ ...form, model_id: e.target.value })}
            placeholder="anthropic/claude-sonnet-4"
            spellCheck={false}
            autoFocus
          />
        </label>
        <label className="field">
          Label
          <input
            type="text"
            value={form.label}
            onChange={(e) => setForm({ ...form, label: e.target.value })}
            placeholder="Sonnet (primary)"
          />
        </label>
        <label className="field">
          Role
          <select
            value={form.role}
            disabled={editing !== null}
            onChange={(e) => setForm({ ...form, role: e.target.value as ModelRole })}
            data-hint="chat = insertion modules and rewriting. embed = the vectors behind semantic matching. A model's role cannot be changed after it is created."
          >
            {MODEL_ROLES.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>
          {editing && (
            <span className="faint" style={{ fontSize: 12 }}>
              Role is fixed once a model exists — delete and re-add to change it.
            </span>
          )}
        </label>
        <label className="field">
          Context tokens
          <input
            type="number"
            min={1}
            value={form.context_tokens}
            onChange={(e) => setForm({ ...form, context_tokens: e.target.value })}
            placeholder="200000"
          />
        </label>
        {form.role === 'embed' && (
          <label className="field">
            Vector width (dim) — required
            <input
              type="number"
              min={1}
              value={form.dim}
              onChange={(e) => setForm({ ...form, dim: e.target.value })}
              placeholder={String(indexDim)}
              data-hint={`The live vector index is ${indexDim}-d. A chain refuses any embed rung of a different width — mixing widths corrupts the index silently.`}
            />
            <span className={dimMismatch ? 'warn-note' : 'faint'} style={{ fontSize: 12 }}>
              {dimMismatch
                ? `${dimValue}-d does not match the live ${indexDim}-d index — this model can be stored, but a chain will refuse it as a rung.`
                : `Must be ${indexDim} to be usable as a chain rung.`}
            </span>
          </label>
        )}
        <label className="field">
          Price in (per 1M tokens)
          <input
            type="number"
            min={0}
            step="0.01"
            value={form.price_in}
            onChange={(e) => setForm({ ...form, price_in: e.target.value })}
            placeholder="3.00"
          />
        </label>
        <label className="field">
          Price out (per 1M tokens)
          <input
            type="number"
            min={0}
            step="0.01"
            value={form.price_out}
            onChange={(e) => setForm({ ...form, price_out: e.target.value })}
            placeholder="15.00"
          />
        </label>
        <label className="field">
          Notes
          <input type="text" value={form.notes} onChange={(e) => setForm({ ...form, notes: e.target.value })} />
        </label>
        <label className="field">
          <span className="dup-toggle">
            <input
              type="checkbox"
              checked={form.enabled}
              onChange={(e) => setForm({ ...form, enabled: e.target.checked })}
            />
            Enabled
          </span>
        </label>
      </div>
    </Modal>
  );
}

// ---------------------------------------------------------------------------
// Probe result
// ---------------------------------------------------------------------------

function ProbeResultModal({
  title,
  result,
  indexDim,
  onClose,
}: {
  title: string;
  result: ProbeResult;
  indexDim: number;
  onClose: () => void;
}) {
  const dim = result.dim ?? null;
  return (
    <Modal
      title={`Probe: ${title}`}
      onClose={onClose}
      footer={
        <button className="btn btn-primary" onClick={onClose}>
          Close
        </button>
      }
    >
      <div className="settings-list">
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Result</div>
            <div className="settings-row-desc">
              {result.ok ? 'The provider answered a live request.' : 'The request did not succeed.'}
            </div>
          </div>
          <StatusChip status={result.ok ? 'done' : 'failed'} />
        </div>
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Latency</div>
            <div className="settings-row-desc mono">{result.latency_ms} ms</div>
          </div>
          {result.status_code !== null && <span className="chip chip-mode">HTTP {result.status_code}</span>}
        </div>
        {dim !== null && (
          <div className="settings-row">
            <div className="settings-row-text">
              <div className="settings-row-label">Vector width returned</div>
              <div className="settings-row-desc mono">{dim}-d</div>
            </div>
            <span className={`chip chip-${dim === (result.index_dim ?? indexDim) ? 'done' : 'failed'}`}>
              {dim === (result.index_dim ?? indexDim)
                ? 'matches the index'
                : `index is ${result.index_dim ?? indexDim}-d`}
            </span>
          </div>
        )}
        {result.sample != null && result.sample !== '' && (
          <div className="settings-row">
            <div className="settings-row-text">
              <div className="settings-row-label">Sample reply</div>
              <div className="settings-row-desc mono">{result.sample}</div>
            </div>
          </div>
        )}
      </div>
      {result.error && <div className="error-box" style={{ marginTop: 14 }}>{result.error}</div>}
    </Modal>
  );
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

export function ModelsPanel() {
  const toast = useToast();
  const [providers, setProviders] = useState<ProviderHealth[] | null>(null);
  const [indexDim, setIndexDim] = useState(DEFAULT_INDEX_DIM);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [providerForm, setProviderForm] = useState<{ initial: ProviderFormState; editing: ModelProvider | null } | null>(
    null,
  );
  const [modelForm, setModelForm] = useState<{
    initial: ModelFormState;
    editing: ModelRow | null;
    providerId: string;
    providerName: string;
  } | null>(null);
  const [probe, setProbe] = useState<{ title: string; result: ProbeResult } | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const health = await fetchProvidersHealth();
      setProviders(health.providers);
      if (typeof health.index_dim === 'number') setIndexDim(health.index_dim);
    } catch (e) {
      const msg =
        e instanceof ApiError && e.status === 403
          ? 'Model settings are restricted to admins.'
          : e instanceof Error
            ? e.message
            : 'Failed to load providers';
      setError(msg);
      setProviders([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const allModels = useMemo(
    () => (providers ?? []).flatMap((p) => p.models),
    [providers],
  );
  const providerNames = useMemo(() => {
    const out: Record<string, string> = {};
    for (const p of providers ?? []) out[p.id] = p.name;
    return out;
  }, [providers]);

  async function run(action: () => Promise<unknown>, okMessage: string) {
    if (busy) return;
    setBusy(true);
    try {
      await action();
      toast.push(okMessage, 'success');
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Request failed', 'error');
    } finally {
      setBusy(false);
    }
  }

  function submitProvider(v: ProviderFormState) {
    const editing = providerForm?.editing ?? null;
    const body: ProviderInput = {
      name: v.name.trim(),
      label: v.label.trim() || v.name.trim(),
      kind: v.kind,
      base_url: v.base_url.trim(),
      enabled: v.enabled,
    };
    // A blank key on EDIT means "keep what is stored" — so the field is omitted
    // entirely rather than sent as '' (which the API reads as "clear the key").
    if (v.api_key !== '' || editing === null) body.api_key = v.api_key;
    void run(async () => {
      if (editing) await patchProvider(editing.id, body);
      else await createProvider(body);
      setProviderForm(null);
    }, editing ? 'Provider updated' : 'Provider added');
  }

  function submitModel(v: ModelFormState) {
    const ctx = numOrUndef(v.context_tokens);
    const dim = numOrUndef(v.dim);
    const priceIn = numOrUndef(v.price_in);
    const priceOut = numOrUndef(v.price_out);
    const editing = modelForm?.editing ?? null;
    void run(async () => {
      if (editing) {
        await patchModel(editing.id, {
          model_id: v.model_id.trim(),
          label: v.label.trim() || v.model_id.trim(),
          context_tokens: ctx,
          dim,
          enabled: v.enabled,
          price_in: priceIn,
          price_out: priceOut,
          notes: v.notes,
        });
      } else {
        const body: ModelInput = {
          provider_id: modelForm?.providerId ?? '',
          model_id: v.model_id.trim(),
          label: v.label.trim() || v.model_id.trim(),
          role: v.role,
          enabled: v.enabled,
        };
        if (ctx !== undefined) body.context_tokens = ctx;
        if (dim !== undefined) body.dim = dim;
        if (priceIn !== undefined) body.price_in = priceIn;
        if (priceOut !== undefined) body.price_out = priceOut;
        if (v.notes.trim()) body.notes = v.notes.trim();
        await createModel(body);
      }
      setModelForm(null);
    }, editing ? 'Model updated' : 'Model added');
  }

  async function doProbe(kind: 'provider' | 'model', id: string, title: string) {
    if (busy) return;
    setBusy(true);
    try {
      const result = kind === 'provider' ? await probeProvider(id) : await probeModel(id);
      setProbe({ title, result });
      toast.push(result.ok ? `${title} responded in ${result.latency_ms}ms` : `${title} probe failed`, result.ok ? 'success' : 'error');
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Probe failed', 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="card">
        <div className="table-toolbar" style={{ marginBottom: 12 }}>
          <div className="card-title" style={{ marginBottom: 0 }}>
            Providers
          </div>
          <div className="spacer" />
          <button
            className="btn btn-primary"
            disabled={busy}
            onClick={() => setProviderForm({ initial: EMPTY_PROVIDER, editing: null })}
            data-hint="Register an LLM endpoint — its kind, base URL and key. Models are added underneath it."
          >
            ＋ Add provider
          </button>
        </div>

        <div className="info-note" style={{ marginBottom: 14 }}>
          <span className="info-note-icon" aria-hidden="true">
            ℹ
          </span>
          <span className="info-note-text">
            Nothing here is required to run SiloForge. With no chat chain configured the system uses the
            environment configuration and everything keeps working — this section is for taking that
            choice out of the env file.
          </span>
        </div>

        {error && <div className="error-box">{error}</div>}

        {providers === null ? (
          <Skeleton rows={6} />
        ) : providers.length === 0 ? (
          <EmptyState title="No providers yet" hint="Add one to start managing models from the UI." />
        ) : (
          <div className="provider-list">
            {providers.map((p) => (
              <div key={p.id} className={`provider-card${p.enabled ? '' : ' disabled'}`}>
                <div className="provider-head">
                  <span className="provider-name">{p.label || p.name}</span>
                  <span className="chip chip-mode">{p.kind}</span>
                  <StatusChip status={p.enabled ? 'approved' : 'reverted'} />
                  <ProbeChip ok={p.last_probe_ok} at={p.last_probe_at} ms={p.last_probe_ms} />
                  <div className="spacer" />
                  <button
                    className="btn btn-sm"
                    disabled={busy}
                    onClick={() => void doProbe('provider', p.id, p.name)}
                    data-hint="Sends one tiny live request through this provider's first enabled model to check the URL and credentials."
                  >
                    Probe
                  </button>
                  <button
                    className="btn btn-sm"
                    disabled={busy}
                    onClick={() =>
                      setProviderForm({
                        initial: {
                          name: p.name,
                          label: p.label,
                          kind: p.kind,
                          base_url: p.base_url,
                          api_key: '',
                          enabled: p.enabled,
                        },
                        editing: p,
                      })
                    }
                    data-hint="Change this provider's URL, kind, key or enabled state."
                  >
                    Edit
                  </button>
                  <button
                    className="btn btn-sm btn-danger"
                    disabled={busy}
                    onClick={() => void run(() => deleteProvider(p.id), 'Provider deleted')}
                    data-hint="Delete this provider and every model under it. Chains referencing those models lose their rungs."
                  >
                    Delete
                  </button>
                </div>

                <div className="provider-meta">
                  <span className="mono-sm" title={p.base_url}>
                    {p.base_url}
                  </span>
                  <span className="faint">·</span>
                  <span className="mono-sm" title="The API never returns the key itself">
                    key: {p.has_api_key ? p.api_key_masked : 'none'}
                  </span>
                  {p.last_probe_error && (
                    <>
                      <span className="faint">·</span>
                      <span style={{ color: 'var(--red)' }} title={p.last_probe_error}>
                        {p.last_probe_error.slice(0, 120)}
                      </span>
                    </>
                  )}
                </div>

                {p.models.length === 0 ? (
                  <div className="muted" style={{ padding: '4px 0' }}>
                    No models yet — a provider with no models cannot be probed or chained.
                  </div>
                ) : (
                  <div className="tbl-wrap">
                    <table className="tbl">
                      <thead>
                        <tr>
                          <th>Model</th>
                          <th style={{ width: 80 }}>Role</th>
                          <th className="num" style={{ width: 90 }}>
                            Context
                          </th>
                          <th className="num" style={{ width: 80 }}>
                            Dim
                          </th>
                          <th className="num" style={{ width: 120 }}>
                            Price in / out
                          </th>
                          <th style={{ width: 140 }}>Last probe</th>
                          <th style={{ width: 170 }}>Actions</th>
                        </tr>
                      </thead>
                      <tbody>
                        {p.models.map((m) => (
                          <tr key={m.id}>
                            <td>
                              <div>{m.label || m.model_id}</div>
                              <div className="mono-sm faint" title={m.model_id}>
                                {m.model_id}
                              </div>
                            </td>
                            <td>
                              <span className="chip chip-mode">{m.role}</span>
                            </td>
                            <td className="num muted">{m.context_tokens ?? '—'}</td>
                            <td className="num">
                              {m.role !== 'embed' ? (
                                <span className="faint">—</span>
                              ) : m.dim === null ? (
                                <span className="chip chip-failed">unset</span>
                              ) : (
                                <span className={`chip chip-${m.dim === indexDim ? 'done' : 'failed'}`}>{m.dim}</span>
                              )}
                            </td>
                            <td className="num muted">
                              {m.price_in ?? '—'} / {m.price_out ?? '—'}
                            </td>
                            <td>
                              <ProbeChip ok={m.last_probe_ok} at={m.last_probe_at} ms={m.last_probe_ms} />
                            </td>
                            <td>
                              <div className="row-actions">
                                <button
                                  className="btn btn-sm"
                                  disabled={busy}
                                  onClick={() => void doProbe('model', m.id, m.model_id)}
                                  data-hint="Sends one live request to this exact model. Embed probes report the vector width the model actually returns."
                                >
                                  Probe
                                </button>
                                <button
                                  className="btn btn-sm"
                                  disabled={busy}
                                  onClick={() =>
                                    setModelForm({
                                      initial: {
                                        model_id: m.model_id,
                                        label: m.label,
                                        role: m.role,
                                        context_tokens: m.context_tokens === null ? '' : String(m.context_tokens),
                                        dim: m.dim === null ? '' : String(m.dim),
                                        price_in: m.price_in === null ? '' : String(m.price_in),
                                        price_out: m.price_out === null ? '' : String(m.price_out),
                                        enabled: m.enabled,
                                        notes: m.notes ?? '',
                                      },
                                      editing: m,
                                      providerId: p.id,
                                      providerName: p.name,
                                    })
                                  }
                                >
                                  Edit
                                </button>
                                <button
                                  className="btn btn-sm btn-danger"
                                  disabled={busy}
                                  onClick={() => void run(() => deleteModel(m.id), 'Model deleted')}
                                >
                                  Delete
                                </button>
                              </div>
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}

                <div className="form-actions">
                  <button
                    className="btn btn-sm"
                    disabled={busy}
                    onClick={() =>
                      setModelForm({
                        initial: EMPTY_MODEL,
                        editing: null,
                        providerId: p.id,
                        providerName: p.name,
                      })
                    }
                    data-hint="Add a model served by this provider. Models are what chains are built from."
                  >
                    ＋ Add model
                  </button>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      <ChainsPanel models={allModels} providerNames={providerNames} indexDim={indexDim} />

      {providerForm && (
        <ProviderForm
          initial={providerForm.initial}
          editing={providerForm.editing}
          busy={busy}
          onCancel={() => setProviderForm(null)}
          onSubmit={submitProvider}
        />
      )}

      {modelForm && (
        <ModelForm
          initial={modelForm.initial}
          editing={modelForm.editing}
          providerName={modelForm.providerName}
          indexDim={indexDim}
          busy={busy}
          onCancel={() => setModelForm(null)}
          onSubmit={submitModel}
        />
      )}

      {probe && (
        <ProbeResultModal
          title={probe.title}
          result={probe.result}
          indexDim={indexDim}
          onClose={() => setProbe(null)}
        />
      )}
    </>
  );
}
