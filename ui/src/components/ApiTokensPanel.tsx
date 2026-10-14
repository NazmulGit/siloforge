import { useCallback, useEffect, useState } from 'react';
import { createToken, fetchTokens, isUnavailable, revokeToken, v2ErrorMessage } from '../api2';
import { fmtDate } from '../format';
import { useToast } from '../toast';
import { useUser } from '../user';
import type { ApiTokenCreated, ApiTokenRow, TokenScope } from '../types2';
import { Modal } from './Modal';
import { Skeleton } from './Skeleton';
import { FeatureUnavailable } from './V2Notice';

const SCOPES: Array<{ id: TokenScope; label: string; hint: string; adminOnly?: boolean }> = [
  { id: 'read', label: 'read', hint: 'GET requests only — reports, edges, jobs.' },
  { id: 'write', label: 'write', hint: 'Create and change things: crawl, plan, approve, inject.' },
  { id: 'admin', label: 'admin', hint: 'Admin routes: models, users, backups, system.', adminOnly: true },
  { id: '*', label: 'all (*)', hint: 'Everything your account can do.' },
];

const EXPIRY: Array<{ days: number | null; label: string }> = [
  { days: 30, label: '30 days' },
  { days: 90, label: '90 days' },
  { days: 365, label: '1 year' },
  { days: null, label: 'Never' },
];

function tokenState(t: ApiTokenRow): { label: string; cls: string } {
  if (t.revoked_at) return { label: 'revoked', cls: 'failed' };
  if (t.expires_at && new Date(t.expires_at).getTime() < Date.now()) return { label: 'expired', cls: 'reverted' };
  return { label: 'active', cls: 'done' };
}

function CreateTokenModal({ onClose, onCreated }: { onClose: () => void; onCreated: (t: ApiTokenCreated) => void }) {
  const { user } = useUser();
  const isAdmin = user?.role === 'admin';
  const [name, setName] = useState('');
  const [scopes, setScopes] = useState<TokenScope[]>(['read', 'write']);
  const [days, setDays] = useState<number | null>(90);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit() {
    setError(null);
    if (!name.trim()) {
      setError('Give the token a name, e.g. "hermes-ap13".');
      return;
    }
    if (scopes.length === 0) {
      setError('Pick at least one scope.');
      return;
    }
    setBusy(true);
    try {
      const t = await createToken({ name: name.trim(), scopes, ...(days ? { expires_in_days: days } : {}) });
      onCreated(t);
    } catch (e) {
      setError(v2ErrorMessage(e, 'Could not create token'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      title="Create API token"
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={busy} onClick={() => void submit()}>
            {busy ? 'Creating…' : 'Create token'}
          </button>
        </>
      }
    >
      <div className="form-grid" style={{ maxWidth: 'none' }}>
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          Name
          <input type="text" value={name} onChange={(e) => setName(e.target.value)} placeholder="hermes-ap13" autoFocus />
        </label>
        <div className="field">
          Scopes
          <div style={{ display: 'flex', flexDirection: 'column', gap: 6, marginTop: 4 }}>
            {SCOPES.filter((s) => !s.adminOnly || isAdmin).map((s) => (
              <label key={s.id} style={{ display: 'inline-flex', gap: 8, alignItems: 'center', fontSize: 13 }}>
                <input
                  type="checkbox"
                  checked={scopes.includes(s.id)}
                  onChange={(e) => setScopes(e.target.checked ? [...scopes, s.id] : scopes.filter((x) => x !== s.id))}
                />
                <span className="mono">{s.label}</span>
                <span className="faint">{s.hint}</span>
              </label>
            ))}
          </div>
        </div>
        <label className="field">
          Expires
          <select value={days ?? ''} onChange={(e) => setDays(e.target.value ? Number(e.target.value) : null)}>
            {EXPIRY.map((x) => (
              <option key={x.label} value={x.days ?? ''}>
                {x.label}
              </option>
            ))}
          </select>
        </label>
      </div>
    </Modal>
  );
}

function ShowOnceModal({ token, onClose }: { token: ApiTokenCreated; onClose: () => void }) {
  const toast = useToast();
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(token.token);
      setCopied(true);
    } catch {
      toast.push('Copy failed — select the token manually', 'error');
    }
  }
  return (
    <Modal
      title="Copy your new token"
      onClose={onClose}
      footer={
        <button className="btn btn-primary" onClick={onClose}>
          I&apos;ve saved it
        </button>
      }
    >
      <p className="muted" style={{ marginTop: 0, fontSize: 13 }}>
        This is the only time the token is shown. Store it somewhere safe (for Hermes: the <code>SILOFORGE_TOKEN</code>{' '}
        env var). It works like a password in the <code>Authorization: Bearer</code> header.
      </p>
      <div className="apikey-box">
        <code>{token.token}</code>
        <button className="btn btn-sm" onClick={() => void copy()}>
          {copied ? 'Copied ✓' : 'Copy'}
        </button>
      </div>
      <div className="faint" style={{ fontSize: 12, marginTop: 10 }}>
        {token.name} · scopes {token.scopes.join(', ')} · {token.expires_at ? `expires ${fmtDate(token.expires_at)}` : 'never expires'}
      </div>
    </Modal>
  );
}

/** Settings → API tokens: personal tokens for Hermes / scripts / MCP. */
export function ApiTokensPanel() {
  const toast = useToast();
  const [rows, setRows] = useState<ApiTokenRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [creating, setCreating] = useState(false);
  const [fresh, setFresh] = useState<ApiTokenCreated | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [showRevoked, setShowRevoked] = useState(false);

  const load = useCallback(async () => {
    try {
      setRows(await fetchTokens());
      setError(null);
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(v2ErrorMessage(e, 'Failed to load tokens'));
      setRows([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function revoke(t: ApiTokenRow) {
    if (!window.confirm(`Revoke "${t.name}"? Anything using it stops working immediately.`)) return;
    setBusy(t.id);
    try {
      await revokeToken(t.id);
      toast.push('Token revoked', 'success');
      await load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Could not revoke token'), 'error');
    } finally {
      setBusy(null);
    }
  }

  if (unavailable) {
    return (
      <div className="card">
        <FeatureUnavailable feature="API tokens" />
      </div>
    );
  }

  const visible = (rows ?? []).filter((t) => showRevoked || !t.revoked_at);

  return (
    <div className="card">
      <div className="table-toolbar" style={{ marginBottom: 12 }}>
        <div className="card-title" style={{ marginBottom: 0 }}>
          API tokens
        </div>
        <div className="spacer" />
        <label className="muted" style={{ display: 'inline-flex', gap: 6, alignItems: 'center', fontSize: 13 }}>
          <input type="checkbox" checked={showRevoked} onChange={(e) => setShowRevoked(e.target.checked)} /> Show revoked
        </label>
        <button className="btn btn-primary btn-sm" onClick={() => setCreating(true)}>
          ＋ Create token
        </button>
      </div>
      {error && <div className="error-box" style={{ marginBottom: 10 }}>{error}</div>}
      {rows === null ? (
        <Skeleton rows={3} />
      ) : visible.length === 0 ? (
        <div className="muted">No tokens. Create one for Hermes Agent, the MCP server or your own scripts.</div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Name</th>
                <th>Token</th>
                <th>Scopes</th>
                <th>Created</th>
                <th>Last used</th>
                <th>Expires</th>
                <th>State</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {visible.map((t) => {
                const st = tokenState(t);
                return (
                  <tr key={t.id}>
                    <td>{t.name}</td>
                    <td className="mono-sm">{t.prefix}…</td>
                    <td className="mono-sm">{t.scopes.join(', ')}</td>
                    <td className="muted">{fmtDate(t.created_at)}</td>
                    <td className="muted">{t.last_used_at ? fmtDate(t.last_used_at) : 'never'}</td>
                    <td className="muted">{t.expires_at ? fmtDate(t.expires_at) : 'never'}</td>
                    <td>
                      <span className={`chip chip-${st.cls}`}>{st.label}</span>
                    </td>
                    <td>
                      {!t.revoked_at && (
                        <button className="btn btn-sm btn-danger" disabled={busy === t.id} onClick={() => void revoke(t)}>
                          Revoke
                        </button>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {creating && (
        <CreateTokenModal
          onClose={() => setCreating(false)}
          onCreated={(t) => {
            setCreating(false);
            setFresh(t);
            void load();
          }}
        />
      )}
      {fresh && <ShowOnceModal token={fresh} onClose={() => setFresh(null)} />}
    </div>
  );
}
