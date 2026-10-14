import { useCallback, useEffect, useState } from 'react';
import {
  createBackup,
  deleteBackup,
  downloadBackup,
  fetchBackups,
  isUnavailable,
  v2ErrorMessage,
  verifyBackup,
} from '../api2';
import { fmtBytes, fmtDate } from '../format';
import { useToast } from '../toast';
import type { BackupKind, BackupRow } from '../types2';
import { Modal } from './Modal';
import { Skeleton } from './Skeleton';
import { FeatureUnavailable } from './V2Notice';

const KINDS: Array<{ id: BackupKind; label: string; hint: string }> = [
  { id: 'full', label: 'Full', hint: 'Database snapshot + config + secrets (secrets only when the archive is encrypted). Use this to move or restore a whole install.' },
  { id: 'db', label: 'Database only', hint: 'Just the SQLite snapshot (sites, posts, links, history).' },
  { id: 'config', label: 'Config only', hint: 'siloforge.toml, plus secrets.env when encrypted — small, for settings changes.' },
];

function statusChip(status: string): string {
  if (status === 'ok') return 'done';
  if (status === 'failed' || status === 'running') return status;
  return 'queued';
}

function CreateBackupModal({ onClose, onCreated }: { onClose: () => void; onCreated: () => void }) {
  const toast = useToast();
  const [kind, setKind] = useState<BackupKind>('full');
  const [note, setNote] = useState('');
  const [passphrase, setPassphrase] = useState('');
  const [confirm, setConfirm] = useState('');
  const [unencrypted, setUnencrypted] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit() {
    setError(null);
    if (unencrypted && passphrase) {
      setError('Clear the passphrase or untick "Unencrypted".');
      return;
    }
    if (passphrase && passphrase.length < 8) {
      setError('Passphrase must be at least 8 characters (or leave it empty).');
      return;
    }
    if (passphrase !== confirm) {
      setError('Passphrase and confirmation do not match.');
      return;
    }
    setBusy(true);
    try {
      await createBackup({
        kind,
        ...(passphrase ? { passphrase } : {}),
        ...(note.trim() ? { note: note.trim() } : {}),
        ...(unencrypted ? { unencrypted: true } : {}),
      });
      toast.push('Backup started', 'success');
      onCreated();
    } catch (e) {
      setError(v2ErrorMessage(e, 'Backup failed to start'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      title="Create backup"
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={busy} onClick={() => void submit()}>
            {busy ? 'Starting…' : 'Create backup'}
          </button>
        </>
      }
    >
      <div className="form-grid" style={{ maxWidth: 'none' }}>
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          What to back up
          <select value={kind} onChange={(e) => setKind(e.target.value as BackupKind)}>
            {KINDS.map((k) => (
              <option key={k.id} value={k.id}>
                {k.label}
              </option>
            ))}
          </select>
          <span className="faint" style={{ fontSize: 12 }}>
            {KINDS.find((k) => k.id === kind)?.hint}
          </span>
        </label>
        <label className="field">
          Note (optional)
          <input type="text" value={note} onChange={(e) => setNote(e.target.value)} placeholder="before cutover" />
        </label>
        <label className="field">
          Passphrase (optional)
          <input type="password" autoComplete="new-password" value={passphrase} onChange={(e) => setPassphrase(e.target.value)} />
          <span className="faint" style={{ fontSize: 12 }}>
            Leave empty to use the server&apos;s configured passphrase (SILOFORGE_BACKUP_PASSPHRASE in secrets.env).
            A passphrase typed here is not stored anywhere — without it you cannot restore. If no passphrase is set at
            all, the archive is unencrypted and secrets.env (master keys) is left out of it.
          </span>
        </label>
        <label className="field" style={{ flexDirection: 'row', alignItems: 'center', gap: 8 }}>
          <input
            type="checkbox"
            checked={unencrypted}
            onChange={(e) => {
              setUnencrypted(e.target.checked);
              if (e.target.checked) {
                setPassphrase('');
                setConfirm('');
              }
            }}
          />
          Unencrypted (ignore the server passphrase; secrets.env is left out)
        </label>
        {passphrase && (
          <label className="field">
            Confirm passphrase
            <input type="password" autoComplete="new-password" value={confirm} onChange={(e) => setConfirm(e.target.value)} />
          </label>
        )}
      </div>
    </Modal>
  );
}

/** Settings → Backups (admin): list, create, verify, download, delete. Restore is CLI-only. */
export function BackupsPanel() {
  const toast = useToast();
  const [rows, setRows] = useState<BackupRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [creating, setCreating] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const list = await fetchBackups();
      setRows(list);
      setError(null);
      return list;
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(v2ErrorMessage(e, 'Failed to load backups'));
      setRows([]);
      return [];
    }
  }, []);

  // Refresh every 5 s while a backup is still being written.
  useEffect(() => {
    let alive = true;
    let timer: number | undefined;
    const tick = async () => {
      const list = await load();
      if (alive && list.some((b) => b.status === 'running' || b.status === 'pending')) {
        timer = window.setTimeout(() => void tick(), 5000);
      }
    };
    void tick();
    return () => {
      alive = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [load]);

  async function verify(b: BackupRow) {
    let passphrase: string | undefined;
    if (b.encrypted) {
      const p = window.prompt("Passphrase for this encrypted backup (leave empty to use the server's configured passphrase):", '');
      if (p === null) return;
      passphrase = p || undefined;
    }
    setBusy(b.id);
    try {
      const r = await verifyBackup(b.id, passphrase);
      if (r.ok) toast.push('Backup verified — archive is intact', 'success');
      else toast.push(`Verify failed: ${r.error ?? 'archive is damaged'}`, 'error');
      await load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Verify failed'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function download(b: BackupRow) {
    setBusy(b.id);
    try {
      const stamp = b.created_at.replace(/[:.]/g, '-').slice(0, 19);
      await downloadBackup(b.id, `siloforge-${b.kind}-${stamp}.zip`);
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Download failed'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function remove(b: BackupRow) {
    if (!window.confirm(`Delete the ${b.kind} backup from ${fmtDate(b.created_at)}? This removes the file.`)) return;
    setBusy(b.id);
    try {
      await deleteBackup(b.id);
      toast.push('Backup deleted', 'success');
      await load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Delete failed'), 'error');
    } finally {
      setBusy(null);
    }
  }

  if (unavailable) {
    return (
      <div className="card">
        <FeatureUnavailable feature="Backups" />
      </div>
    );
  }

  return (
    <>
      <div className="card">
        <div className="table-toolbar" style={{ marginBottom: 12 }}>
          <div className="card-title" style={{ marginBottom: 0 }}>
            Backups
          </div>
          <div className="spacer" />
          <button className="btn btn-sm" onClick={() => void load()}>
            Refresh
          </button>
          <button className="btn btn-primary btn-sm" onClick={() => setCreating(true)}>
            ＋ Create backup
          </button>
        </div>
        {error && <div className="error-box" style={{ marginBottom: 10 }}>{error}</div>}
        {rows === null ? (
          <Skeleton rows={3} />
        ) : rows.length === 0 ? (
          <div className="muted">No backups yet.</div>
        ) : (
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>Created</th>
                  <th>Kind</th>
                  <th className="num">Size</th>
                  <th>Encrypted</th>
                  <th>Version</th>
                  <th>Status</th>
                  <th>Note</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((b) => (
                  <tr key={b.id}>
                    <td className="muted">{fmtDate(b.created_at)}</td>
                    <td className="mono">{b.kind}</td>
                    <td className="num">{fmtBytes(b.size_bytes)}</td>
                    <td>{b.encrypted ? 'yes' : 'no'}</td>
                    <td className="mono-sm">{b.app_version || '—'}</td>
                    <td>
                      <span className={`chip chip-${statusChip(b.status ?? 'ok')}`}>{b.status ?? 'ok'}</span>
                    </td>
                    <td className="muted" style={{ maxWidth: 200, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={b.note}>
                      {b.note || '—'}
                    </td>
                    <td>
                      <div className="row-actions">
                        <button className="btn btn-sm" disabled={busy === b.id} onClick={() => void verify(b)} data-hint="Re-read the archive and check its checksum.">
                          Verify
                        </button>
                        <button className="btn btn-sm" disabled={busy === b.id || (b.status ?? 'ok') !== 'ok'} onClick={() => void download(b)}>
                          Download
                        </button>
                        <button className="btn btn-sm btn-danger" disabled={busy === b.id} onClick={() => void remove(b)}>
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
      </div>

      <div className="card">
        <div className="card-title">Restore</div>
        <p className="muted" style={{ marginTop: 0, fontSize: 13 }}>
          Restoring replaces the whole database, so it is done from the command line with the server stopped — never from
          the browser. Stop SiloForge, then run:
        </p>
        <div className="apikey-box">
          <code>siloforge restore &lt;file&gt;</code>
          <button
            className="btn btn-sm"
            onClick={() => {
              void navigator.clipboard.writeText('siloforge restore ').then(
                () => toast.push('Copied', 'success'),
                () => toast.push('Copy failed', 'error'),
              );
            }}
          >
            Copy
          </button>
        </div>
        <ul className="muted" style={{ fontSize: 13, marginBottom: 0 }}>
          <li>
            <code>&lt;file&gt;</code> is a downloaded backup archive (or one from the data folder&apos;s <code>backups\</code>).
          </li>
          <li>
            Encrypted archives need the passphrase used to create them (by default SILOFORGE_BACKUP_PASSPHRASE from the old
            install&apos;s <code>secrets.env</code> — keep a copy of it outside this machine).
          </li>
          <li>
            Unencrypted archives do not contain <code>secrets.env</code>: copy the original file into the data folder before
            starting SiloForge, or stored WordPress/GSC credentials cannot be decrypted.
          </li>
          <li>Start SiloForge again afterwards.</li>
        </ul>
      </div>

      {creating && (
        <CreateBackupModal
          onClose={() => setCreating(false)}
          onCreated={() => {
            setCreating(false);
            void load();
          }}
        />
      )}
    </>
  );
}
