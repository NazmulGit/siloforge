// Admin → Invites (all workspaces) and Admin → Email (SMTP + test + outbox).

import { useCallback, useEffect, useState } from 'react';
import { absoluteLink, fmtDate } from '../../format';
import {
  deleteOutboxRow,
  fetchAdminEmail,
  fetchAdminInvites,
  putPlatform,
  revokeInvite,
  sendTestEmail,
  type InviteRow,
  type OutboxRow,
  type SmtpSettings,
} from '../../saas';
import { useToast } from '../../toast';
import { useUser } from '../../user';
import { CopyButton } from '../CopyButton';
import { Skeleton } from '../Skeleton';
import { errText } from './common';

export function AdminInvites() {
  const toast = useToast();
  const [rows, setRows] = useState<InviteRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showDone, setShowDone] = useState(false);

  const load = useCallback(async () => {
    try {
      setRows(await fetchAdminInvites());
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load invitations'));
      setRows([]);
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  async function revoke(i: InviteRow) {
    if (!i.workspace_id || !window.confirm(`Revoke the invitation for ${i.email}?`)) return;
    try {
      await revokeInvite(i.workspace_id, i.id);
      toast.push('Invitation revoked', 'success');
      await load();
    } catch (e) {
      toast.push(errText(e, 'Could not revoke'), 'error');
    }
  }

  const visible = (rows ?? []).filter((i) => showDone || !i.accepted_at);
  return (
    <div className="card">
      <div className="table-toolbar">
        <div className="muted">Invitations across every workspace.</div>
        <div className="spacer" />
        <label className="check-line muted">
          <input type="checkbox" checked={showDone} onChange={(e) => setShowDone(e.target.checked)} /> Show accepted
        </label>
      </div>
      {error && <div className="error-box">{error}</div>}
      {rows === null ? (
        <Skeleton rows={4} />
      ) : visible.length === 0 ? (
        <div className="empty">
          <div className="empty-title">No open invitations</div>
          <div className="empty-hint">Workspace admins invite people from Workspace, Members & invites.</div>
        </div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Email</th>
                <th>Workspace</th>
                <th>Role</th>
                <th>Sent</th>
                <th>Expires</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {visible.map((i) => {
                const expired = new Date(i.expires_at).getTime() < Date.now();
                return (
                  <tr key={i.id}>
                    <td>{i.email}</td>
                    <td>{i.workspace_name ?? i.workspace_id ?? '—'}</td>
                    <td>{i.role}</td>
                    <td className="muted">{i.created_at ? fmtDate(i.created_at) : '—'}</td>
                    <td className={expired && !i.accepted_at ? 'tone-bad' : 'muted'}>
                      {i.accepted_at ? `accepted ${fmtDate(i.accepted_at)}` : expired ? 'expired' : fmtDate(i.expires_at)}
                    </td>
                    <td>
                      <div className="row-actions">
                        {i.link && <CopyButton text={absoluteLink(i.link)} label="Copy link" what="Invitation link" />}
                        {!i.accepted_at && i.workspace_id && (
                          <button className="btn btn-sm btn-danger" onClick={() => void revoke(i)}>
                            Revoke
                          </button>
                        )}
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

const BLANK_SMTP: SmtpSettings = { host: '', port: 587, tls: 'starttls', username: '', password: '', from_email: '', from_name: '' };

export function AdminEmail() {
  const toast = useToast();
  const { user } = useUser();
  const [configured, setConfigured] = useState<boolean | null>(null);
  const [smtp, setSmtp] = useState<SmtpSettings>(BLANK_SMTP);
  const [outbox, setOutbox] = useState<OutboxRow[] | null>(null);
  const [testTo, setTestTo] = useState(user?.email ?? '');
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const r = await fetchAdminEmail();
      setConfigured(r.configured);
      setSmtp({ ...BLANK_SMTP, ...r.smtp, port: Number(r.smtp.port ?? 587) || 587 });
      setOutbox(r.outbox ?? []);
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load email settings'));
      setOutbox([]);
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);
  useEffect(() => {
    if (!testTo && user?.email) setTestTo(user.email);
  }, [user?.email, testTo]);

  function set<K extends keyof SmtpSettings>(k: K, v: SmtpSettings[K]) {
    setSmtp((s) => ({ ...s, [k]: v }));
  }

  async function save() {
    setBusy('save');
    try {
      await putPlatform({ smtp });
      toast.push('Mail settings saved', 'success');
      await load();
    } catch (e) {
      toast.push(errText(e, 'Could not save'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function test() {
    if (!testTo.trim()) return;
    setBusy('test');
    try {
      const r = await sendTestEmail(testTo.trim());
      if (r.ok && r.delivery === 'smtp') toast.push(`Test email sent to ${testTo.trim()}`, 'success');
      else if (r.ok) toast.push('No SMTP server is set, so the test landed in the outbox below.', 'info');
      else toast.push(`Sending failed: ${r.error ?? 'unknown error'}`, 'error');
      await load();
    } catch (e) {
      toast.push(errText(e, 'Test failed'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function drop(o: OutboxRow) {
    try {
      await deleteOutboxRow(o.id);
      setOutbox((rows) => (rows ?? []).filter((r) => r.id !== o.id));
    } catch (e) {
      toast.push(errText(e, 'Could not delete'), 'error');
    }
  }

  return (
    <>
      <div className="card">
        <div className="table-toolbar">
          <div className="card-title" style={{ marginBottom: 0 }}>
            SMTP server
          </div>
          {configured !== null && <span className={`chip chip-${configured ? 'done' : 'reverted'}`}>{configured ? 'configured' : 'not configured'}</span>}
        </div>
        <p className="muted card-lead">
          Used for verification, password reset and invitation emails. Without it, every message waits in the outbox below and you
          pass the links on yourself.
        </p>
        {error && <div className="error-box">{error}</div>}
        <div className="form-cols">
          <label className="field">
            Host
            <input value={smtp.host} onChange={(e) => set('host', e.target.value)} placeholder="smtp.example.com" />
          </label>
          <label className="field">
            Port
            <input type="number" value={smtp.port} onChange={(e) => set('port', Number(e.target.value) || 0)} />
          </label>
          <label className="field">
            Encryption
            <select value={smtp.tls} onChange={(e) => set('tls', e.target.value as SmtpSettings['tls'])}>
              <option value="starttls">STARTTLS (port 587)</option>
              <option value="tls">TLS (port 465)</option>
              <option value="none">None (local relay only)</option>
            </select>
          </label>
          <label className="field">
            Username
            <input value={smtp.username} onChange={(e) => set('username', e.target.value)} autoComplete="off" />
          </label>
          <label className="field">
            Password
            <input
              type="password"
              autoComplete="new-password"
              value={smtp.password}
              onChange={(e) => set('password', e.target.value)}
            />
            <span className="field-note faint">Write-only. Leave as is to keep the saved password; clear it and save to remove it.</span>
          </label>
          <label className="field">
            From address
            <input type="email" value={smtp.from_email} onChange={(e) => set('from_email', e.target.value)} placeholder="no-reply@example.com" />
          </label>
          <label className="field">
            From name
            <input value={smtp.from_name} onChange={(e) => set('from_name', e.target.value)} placeholder="SiloForge" />
          </label>
        </div>
        <div className="form-actions">
          <button className="btn btn-primary" disabled={busy !== null} onClick={() => void save()}>
            {busy === 'save' ? 'Saving…' : 'Save mail settings'}
          </button>
        </div>
        <div className="test-mail">
          <label className="field">
            Send a test email to
            <input type="email" value={testTo} onChange={(e) => setTestTo(e.target.value)} />
          </label>
          <button className="btn" disabled={busy !== null || !testTo.trim()} onClick={() => void test()}>
            {busy === 'test' ? 'Sending…' : 'Send test email'}
          </button>
        </div>
      </div>

      <div className="card">
        <div className="card-title">Outbox</div>
        <p className="muted card-lead">Messages that were not sent by SMTP, newest first. Copy a link and send it to the person yourself.</p>
        {outbox === null ? (
          <Skeleton rows={3} />
        ) : outbox.length === 0 ? (
          <div className="muted">The outbox is empty.</div>
        ) : (
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>To</th>
                  <th>Message</th>
                  <th>Created</th>
                  <th>State</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {outbox.map((o) => (
                  <tr key={o.id}>
                    <td>{o.to}</td>
                    <td>
                      <div>{o.subject}</div>
                      <div className="faint field-note">{o.kind.replace(/_/g, ' ')}</div>
                    </td>
                    <td className="muted">{fmtDate(o.created_at)}</td>
                    <td>
                      {o.sent_at ? (
                        <span className="chip chip-done">sent</span>
                      ) : o.error ? (
                        <span className="chip chip-failed" title={o.error}>
                          failed
                        </span>
                      ) : (
                        <span className="chip chip-reverted">manual</span>
                      )}
                    </td>
                    <td>
                      <div className="row-actions">
                        {o.link && <CopyButton text={absoluteLink(o.link)} label="Copy link" what="Link" />}
                        <button className="btn btn-sm" onClick={() => void drop(o)}>
                          Remove
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
    </>
  );
}
