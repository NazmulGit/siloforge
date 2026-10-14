// Settings → Account (v2.4): profile, email change, password, two-factor sign-in, sessions.
// On a pre-2.4 server only the identity row and the password form are shown.

import { useCallback, useEffect, useState, type FormEvent } from 'react';
import { useNavigate } from 'react-router-dom';
import { changePassword, clearToken } from '../../api';
import { saveBlob } from '../../api2';
import { fmtDate } from '../../format';
import {
  changeEmail,
  disableTwoFactor,
  enableTwoFactor,
  fetchSessions,
  logoutAll,
  patchMe,
  regenerateRecoveryCodes,
  revokeSession,
  setupTwoFactor,
  type SessionRow,
} from '../../saas';
import { useToast } from '../../toast';
import { useUser } from '../../user';
import { CopyButton } from '../CopyButton';
import { Modal } from '../Modal';
import { QrCode } from '../QrCode';
import { Skeleton } from '../Skeleton';

function errText(e: unknown, fallback: string): string {
  return e instanceof Error && e.message ? e.message : fallback;
}

function ProfileCard() {
  const { user, refresh } = useUser();
  const toast = useToast();
  const [name, setName] = useState(user?.name ?? '');
  const [busy, setBusy] = useState(false);
  useEffect(() => setName(user?.name ?? ''), [user?.name]);
  const v24 = user?.email_verified !== undefined;

  async function save(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    try {
      await patchMe({ name: name.trim() });
      await refresh();
      toast.push('Name saved', 'success');
    } catch (err) {
      toast.push(errText(err, 'Could not save'), 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="card">
      <div className="card-title">Account</div>
      <div className="settings-list">
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Signed in as</div>
            <div className="settings-row-desc mono">
              {user?.email ?? '—'}
              {user?.email_verified === false && <span className="chip chip-reverted"> unverified</span>}
            </div>
          </div>
          {user && (
            <span className={`role-chip role-${user.role}`} data-hint="Platform admins manage every workspace and the server; users work in the workspaces they belong to.">
              {user.role === 'admin' ? 'platform admin' : user.role}
            </span>
          )}
        </div>
      </div>
      {v24 && (
        <form className="inline-form" onSubmit={(e) => void save(e)}>
          <label className="field">
            Display name
            <input value={name} onChange={(e) => setName(e.target.value)} maxLength={80} autoComplete="name" />
          </label>
          <button className="btn" disabled={busy || name.trim() === (user?.name ?? '')}>
            {busy ? 'Saving…' : 'Save name'}
          </button>
        </form>
      )}
    </div>
  );
}

function EmailCard() {
  const { user, refresh } = useUser();
  const toast = useToast();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    if (!email.trim() || !password) return setError('Enter the new address and your current password.');
    setBusy(true);
    try {
      const r = await changeEmail(email.trim(), password);
      toast.push(
        r.delivery === 'manual'
          ? 'Change requested. No mail server is set up: ask your platform admin for the confirmation link.'
          : `Confirmation link sent to ${r.pending_email}`,
        'success',
      );
      setEmail('');
      setPassword('');
      await refresh();
    } catch (err) {
      setError(errText(err, 'Could not change the email'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="card">
      <div className="card-title">Email address</div>
      {user?.pending_email && (
        <div className="info-note" style={{ marginBottom: 12 }}>
          <span className="info-note-text">
            Waiting for confirmation of <b>{user.pending_email}</b>. Your sign-in address stays {user.email} until you open the link.
          </span>
        </div>
      )}
      <form className="form-grid" onSubmit={(e) => void submit(e)}>
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          New email
          <input type="email" value={email} onChange={(e) => setEmail(e.target.value)} autoComplete="email" />
        </label>
        <label className="field">
          Current password
          <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" />
        </label>
        <div className="form-actions">
          <button className="btn btn-primary" disabled={busy}>
            {busy ? 'Sending…' : 'Change email'}
          </button>
        </div>
      </form>
    </div>
  );
}

function PasswordCard() {
  const toast = useToast();
  const [current, setCurrent] = useState('');
  const [next, setNext] = useState('');
  const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    if (next.length < 8) return setError('New password must be at least 8 characters.');
    if (next !== confirm) return setError('New password and confirmation do not match.');
    if (!current) return setError('Enter your current password.');
    setBusy(true);
    try {
      await changePassword(current, next);
      toast.push('Password updated. Other devices were signed out.', 'success');
      setCurrent('');
      setNext('');
      setConfirm('');
    } catch (err) {
      setError(errText(err, 'Could not update password'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="card">
      <div className="card-title">Change password</div>
      <form className="form-grid" onSubmit={(e) => void submit(e)}>
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          Current password
          <input type="password" autoComplete="current-password" value={current} onChange={(e) => setCurrent(e.target.value)} />
        </label>
        <label className="field">
          New password
          <input type="password" autoComplete="new-password" value={next} onChange={(e) => setNext(e.target.value)} />
          <span className="faint" style={{ fontSize: 12 }}>
            At least 8 characters.
          </span>
        </label>
        <label className="field">
          Confirm new password
          <input type="password" autoComplete="new-password" value={confirm} onChange={(e) => setConfirm(e.target.value)} />
        </label>
        <div className="form-actions">
          <button type="submit" className="btn btn-primary" disabled={busy}>
            {busy ? 'Updating…' : 'Update password'}
          </button>
        </div>
      </form>
    </div>
  );
}

function RecoveryCodes({ codes, onClose }: { codes: string[]; onClose: () => void }) {
  const text = codes.join('\n');
  return (
    <Modal
      title="Save your recovery codes"
      onClose={onClose}
      footer={
        <button className="btn btn-primary" onClick={onClose}>
          I have saved them
        </button>
      }
    >
      <p className="muted" style={{ marginTop: 0 }}>
        Each code signs you in once if you lose your phone. This is the only time they are shown. Keep them somewhere safe, away from
        your password.
      </p>
      <ol className="recovery-codes">
        {codes.map((c) => (
          <li key={c} className="mono">
            {c}
          </li>
        ))}
      </ol>
      <div className="row-actions">
        <CopyButton text={text} label="Copy all" what="Recovery codes" />
        <button className="btn btn-sm" onClick={() => saveBlob(new Blob([`${text}\n`], { type: 'text/plain' }), 'siloforge-recovery-codes.txt')}>
          Download .txt
        </button>
      </div>
    </Modal>
  );
}

function TwoFactorCard() {
  const { user, refresh } = useUser();
  const toast = useToast();
  const enabled = user?.two_factor_enabled === true;
  const [step, setStep] = useState<'idle' | 'password' | 'scan' | 'disable' | 'regen'>('idle');
  const [password, setPassword] = useState('');
  const [code, setCode] = useState('');
  const [setup, setSetup] = useState<{ secret: string; otpauth_uri: string } | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function reset() {
    setStep('idle');
    setPassword('');
    setCode('');
    setSetup(null);
    setError(null);
  }

  async function run(fn: () => Promise<void>) {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(errText(e, 'That did not work'));
    } finally {
      setBusy(false);
    }
  }

  const start = () =>
    run(async () => {
      setSetup(await setupTwoFactor(password));
      setPassword('');
      setStep('scan');
    });

  const confirm = () =>
    run(async () => {
      const r = await enableTwoFactor(code.trim());
      setCodes(r.recovery_codes);
      reset();
      await refresh();
      toast.push('Two-factor sign-in is on', 'success');
    });

  const disable = () =>
    run(async () => {
      await disableTwoFactor(password, code.trim());
      reset();
      await refresh();
      toast.push('Two-factor sign-in is off', 'success');
    });

  const regen = () =>
    run(async () => {
      const r = await regenerateRecoveryCodes(password);
      setCodes(r.recovery_codes);
      reset();
      await refresh();
    });

  return (
    <div className="card">
      <div className="table-toolbar">
        <div className="card-title" style={{ marginBottom: 0 }}>
          Two-factor sign-in
        </div>
        <span className={`chip chip-${enabled ? 'done' : 'reverted'}`}>{enabled ? 'on' : 'off'}</span>
      </div>
      <p className="muted card-lead">
        {enabled
          ? `Signing in asks for a code from your authenticator app. ${user?.recovery_codes_left ?? 0} recovery codes left.`
          : 'Ask for a 6-digit code from an authenticator app (1Password, Google Authenticator, Aegis…) after your password. Strongly recommended for admins.'}
      </p>
      {error && <div className="error-box">{error}</div>}

      {step === 'idle' &&
        (enabled ? (
          <div className="row-actions">
            <button className="btn" onClick={() => setStep('regen')}>
              New recovery codes
            </button>
            <button className="btn btn-danger" onClick={() => setStep('disable')}>
              Turn off
            </button>
          </div>
        ) : (
          <button className="btn btn-primary" onClick={() => setStep('password')}>
            Set up two-factor sign-in
          </button>
        ))}

      {(step === 'password' || step === 'regen') && (
        <form
          className="inline-form"
          onSubmit={(e) => {
            e.preventDefault();
            void (step === 'password' ? start() : regen());
          }}
        >
          <label className="field">
            Confirm your password
            <input type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} autoFocus />
          </label>
          <button className="btn btn-primary" disabled={busy || !password}>
            {busy ? 'Checking…' : step === 'password' ? 'Continue' : 'Create new codes'}
          </button>
          <button type="button" className="btn" onClick={reset}>
            Cancel
          </button>
        </form>
      )}

      {step === 'scan' && setup && (
        <div className="totp-setup">
          <QrCode text={setup.otpauth_uri} label="QR code for your authenticator app" />
          <div className="totp-steps">
            <p>Scan the code with your authenticator app. No camera? Enter this key by hand:</p>
            <div className="apikey-box">
              <code>{setup.secret.replace(/(.{4})/g, '$1 ').trim()}</code>
              <CopyButton text={setup.secret} what="Secret key" />
            </div>
            <form
              className="inline-form"
              onSubmit={(e) => {
                e.preventDefault();
                void confirm();
              }}
            >
              <label className="field">
                Code from the app
                <input
                  className="code-input otp"
                  inputMode="numeric"
                  autoComplete="one-time-code"
                  maxLength={6}
                  value={code}
                  onChange={(e) => setCode(e.target.value.replace(/[^0-9]/g, ''))}
                  autoFocus
                />
              </label>
              <button className="btn btn-primary" disabled={busy || code.length !== 6}>
                {busy ? 'Checking…' : 'Turn on'}
              </button>
              <button type="button" className="btn" onClick={reset}>
                Cancel
              </button>
            </form>
          </div>
        </div>
      )}

      {step === 'disable' && (
        <form
          className="inline-form"
          onSubmit={(e) => {
            e.preventDefault();
            void disable();
          }}
        >
          <label className="field">
            Password
            <input type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} autoFocus />
          </label>
          <label className="field">
            Code or recovery code
            <input autoComplete="one-time-code" value={code} onChange={(e) => setCode(e.target.value)} />
          </label>
          <button className="btn btn-danger" disabled={busy || !password || !code.trim()}>
            {busy ? 'Turning off…' : 'Turn off two-factor'}
          </button>
          <button type="button" className="btn" onClick={reset}>
            Cancel
          </button>
        </form>
      )}

      {codes && <RecoveryCodes codes={codes} onClose={() => setCodes(null)} />}
    </div>
  );
}

function shortAgent(ua: string | null): string {
  if (!ua) return 'Unknown device';
  const browser = /Edg\//.test(ua) ? 'Edge' : /Firefox\//.test(ua) ? 'Firefox' : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : null;
  const os = /Windows/.test(ua) ? 'Windows' : /Mac OS X/.test(ua) ? 'macOS' : /Android/.test(ua) ? 'Android' : /iPhone|iPad/.test(ua) ? 'iOS' : /Linux/.test(ua) ? 'Linux' : null;
  if (browser || os) return [browser, os].filter(Boolean).join(' on ');
  return ua.length > 60 ? `${ua.slice(0, 60)}…` : ua;
}

function SessionsCard() {
  const toast = useToast();
  const navigate = useNavigate();
  const [rows, setRows] = useState<SessionRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setRows((await fetchSessions()).filter((s) => !s.revoked_at));
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load sessions'));
      setRows([]);
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  async function revoke(s: SessionRow) {
    setBusy(s.id);
    try {
      await revokeSession(s.id);
      if (s.current) {
        clearToken();
        navigate('/login');
        return;
      }
      toast.push('Session signed out', 'success');
      await load();
    } catch (e) {
      toast.push(errText(e, 'Could not sign out that session'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function everywhere() {
    if (!window.confirm('Sign out on every device, including this one?')) return;
    setBusy('all');
    try {
      await logoutAll();
    } catch {
      /* the token may already be dead; sign out locally anyway */
    }
    clearToken();
    navigate('/login');
  }

  return (
    <div className="card">
      <div className="table-toolbar">
        <div className="card-title" style={{ marginBottom: 0 }}>
          Where you are signed in
        </div>
        <div className="spacer" />
        <button className="btn btn-sm btn-danger" disabled={busy === 'all'} onClick={() => void everywhere()}>
          Sign out everywhere
        </button>
      </div>
      {error && <div className="error-box">{error}</div>}
      {rows === null ? (
        <Skeleton rows={3} />
      ) : rows.length === 0 ? (
        <div className="muted">No sessions recorded. Sessions started before this version do not show here.</div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Device</th>
                <th>IP</th>
                <th>Started</th>
                <th>Last active</th>
                <th>Expires</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rows.map((s) => (
                <tr key={s.id}>
                  <td>
                    {shortAgent(s.user_agent)}
                    {s.current && <span className="chip chip-done"> this device</span>}
                  </td>
                  <td className="mono-sm">{s.ip ?? '—'}</td>
                  <td className="muted">{fmtDate(s.created_at)}</td>
                  <td className="muted">{s.last_seen_at ? fmtDate(s.last_seen_at) : '—'}</td>
                  <td className="muted">{s.expires_at ? fmtDate(s.expires_at) : '—'}</td>
                  <td>
                    <button className="btn btn-sm" disabled={busy === s.id} onClick={() => void revoke(s)}>
                      Sign out
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

export function AccountPanel() {
  const { user } = useUser();
  // v2.4 servers send email_verified / two_factor_enabled on /auth/me.
  const v24 = user?.email_verified !== undefined || user?.two_factor_enabled !== undefined;
  return (
    <>
      <ProfileCard />
      {v24 && <EmailCard />}
      <PasswordCard />
      {v24 && <TwoFactorCard />}
      {v24 && <SessionsCard />}
    </>
  );
}
