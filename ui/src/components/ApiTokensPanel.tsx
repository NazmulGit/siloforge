import { useState } from 'react';
import { createToken, v2ErrorMessage } from '../api2';
import { fmtDate } from '../format';
import { useToast } from '../toast';
import { useUser } from '../user';
import type { ApiTokenCreated, ApiTokenRow, TokenScope } from '../types2';
import { Modal } from './Modal';

export const SCOPES: Array<{ id: TokenScope; label: string; hint: string; adminOnly?: boolean }> = [
  { id: 'read', label: 'read', hint: 'GET requests only — reports, edges, jobs.' },
  { id: 'write', label: 'write', hint: 'Create and change things: crawl, plan, approve, inject.' },
  { id: 'admin', label: 'admin', hint: 'Admin routes: models, users, backups, system.', adminOnly: true },
  { id: '*', label: 'all (*)', hint: 'Everything your account can do.' },
];

export const EXPIRY: Array<{ days: number | null; label: string }> = [
  { days: 30, label: '30 days' },
  { days: 90, label: '90 days' },
  { days: 365, label: '1 year' },
  { days: null, label: 'Never' },
];

export function tokenState(t: ApiTokenRow): { label: string; cls: string } {
  if (t.revoked_at) return { label: 'revoked', cls: 'failed' };
  if (t.expires_at && new Date(t.expires_at).getTime() < Date.now()) return { label: 'expired', cls: 'reverted' };
  return { label: 'active', cls: 'done' };
}

export function CreateTokenModal({
  onClose,
  onCreated,
  workspaceId,
}: {
  onClose: () => void;
  onCreated: (t: ApiTokenCreated) => void;
  /** v2.4: the workspace the token belongs to (default = current workspace). */
  workspaceId?: string;
}) {
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
      const t = await createToken({ name: name.trim(), scopes, ...(days ? { expires_in_days: days } : {}), ...(workspaceId ? { workspace_id: workspaceId } : {}) });
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

export function ShowOnceModal({ token, onClose }: { token: ApiTokenCreated; onClose: () => void }) {
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

