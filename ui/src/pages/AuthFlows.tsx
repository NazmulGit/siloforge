// Forgot password, reset password, verify email and invitation pages (v2.4 §9.3/§9.4).

import { useEffect, useRef, useState, type FormEvent } from 'react';
import { Link, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { ApiError, getToken } from '../api';
import { useBranding } from '../branding';
import { acceptInvite, fetchInviteInfo, forgotPassword, resetPassword, verifyEmail, type InviteInfo } from '../saas';
import { useUser } from '../user';
import { writeViewAs, writeWorkspaceId } from '../wsStore';
import { AuthLoading, AuthShell, PasswordField } from './auth/AuthShell';

function msg(e: unknown, fallback: string): string {
  return e instanceof Error && e.message ? e.message : fallback;
}

export function ForgotPage() {
  const { config, branding } = useBranding();
  const [email, setEmail] = useState('');
  const [busy, setBusy] = useState(false);
  const [sent, setSent] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const support = branding.support_email.trim();

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (!email.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      await forgotPassword(email.trim());
      setSent(true);
    } catch (err) {
      setError(err instanceof ApiError && err.status === 429 ? 'Too many requests from this network. Try again in a while.' : msg(err, 'Could not send the link'));
    } finally {
      setBusy(false);
    }
  }

  if (!config) {
    return (
      <AuthShell title="Reset your password">
        <AuthLoading />
      </AuthShell>
    );
  }

  if (sent) {
    return (
      <AuthShell title="Check your email">
        <div className="auth-form">
          <p className="auth-body">
            If an account exists for <b>{email.trim()}</b>, a reset link is on its way. The link works once and expires in one hour.
          </p>
          {!config.email_enabled && (
            <div className="auth-note">
              This server does not send email yet. Your platform admin{support ? ` (${support})` : ''} can copy the link from
              Admin, Email and send it to you.
            </div>
          )}
          <Link className="btn btn-block" to="/login">
            Back to sign in
          </Link>
        </div>
      </AuthShell>
    );
  }

  return (
    <AuthShell title="Reset your password" lead="Enter the email you sign in with. We will send a link to choose a new password.">
      <form className="auth-form" onSubmit={(e) => void submit(e)}>
        {!config.email_enabled && (
          <div className="auth-note">
            Email is not set up on this server, so the link goes to your platform admin{support ? ` (${support})` : ''} instead. Ask
            them to pass it on.
          </div>
        )}
        {error && <div className="error-box">{error}</div>}
        <label className="field auth-field">
          Email
          <input type="email" value={email} onChange={(e) => setEmail(e.target.value)} autoComplete="email" placeholder="you@example.com" autoFocus />
        </label>
        <button type="submit" className="btn btn-primary btn-block" disabled={busy || !email.trim()}>
          {busy ? 'Sending…' : 'Send reset link'}
        </button>
      </form>
      <div className="auth-switch">
        Remembered it? <Link to="/login">Sign in</Link>
      </div>
    </AuthShell>
  );
}

export function ResetPage() {
  const [params] = useSearchParams();
  const token = params.get('token') ?? '';
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy) return;
    if (password.length < 8) return setError('Use at least 8 characters for the password.');
    if (password !== confirm) return setError('The two passwords do not match.');
    setBusy(true);
    setError(null);
    try {
      await resetPassword(token, password);
      setDone(true);
    } catch (err) {
      setError(
        err instanceof ApiError && (err.status === 400 || err.status === 404 || err.status === 410)
          ? 'This reset link has expired or was already used. Request a new one.'
          : msg(err, 'Could not reset the password'),
      );
    } finally {
      setBusy(false);
    }
  }

  if (!token) {
    return (
      <AuthShell title="Reset link missing">
        <div className="auth-form">
          <p className="auth-body">Open the full link from the email. If it no longer works, request a new one.</p>
          <Link className="btn btn-primary btn-block" to="/forgot">
            Request a new link
          </Link>
        </div>
      </AuthShell>
    );
  }

  if (done) {
    return (
      <AuthShell title="Password changed">
        <div className="auth-form">
          <p className="auth-body">Your new password is set. For safety, every device that was signed in has been signed out.</p>
          <Link className="btn btn-primary btn-block" to="/login">
            Sign in
          </Link>
        </div>
      </AuthShell>
    );
  }

  return (
    <AuthShell title="Choose a new password">
      <form className="auth-form" onSubmit={(e) => void submit(e)}>
        {error && (
          <div className="error-box">
            {error} {/expired|used/.test(error) && <Link to="/forgot">Request a new link</Link>}
          </div>
        )}
        <PasswordField label="New password" value={password} onChange={setPassword} autoComplete="new-password" autoFocus note="At least 8 characters." />
        <PasswordField label="Confirm new password" value={confirm} onChange={setConfirm} autoComplete="new-password" />
        <button type="submit" className="btn btn-primary btn-block" disabled={busy || !password || !confirm}>
          {busy ? 'Saving…' : 'Set new password'}
        </button>
      </form>
    </AuthShell>
  );
}

export function VerifyEmailPage() {
  const [params] = useSearchParams();
  const token = params.get('token') ?? '';
  const { refresh } = useUser();
  const [state, setState] = useState<{ kind: 'busy' } | { kind: 'ok'; email: string } | { kind: 'error'; text: string }>({ kind: 'busy' });
  const ran = useRef(false);

  useEffect(() => {
    if (ran.current) return; // the link is single-use; StrictMode would run this twice
    ran.current = true;
    if (!token) {
      setState({ kind: 'error', text: 'The confirmation link is incomplete. Open the full link from the email.' });
      return;
    }
    verifyEmail(token)
      .then((r) => {
        setState({ kind: 'ok', email: r.email });
        if (getToken()) void refresh();
      })
      .catch((e: unknown) =>
        setState({
          kind: 'error',
          text:
            e instanceof ApiError && e.status < 500
              ? 'This confirmation link has expired or was already used. Sign in and choose "Resend email" for a new one.'
              : msg(e, 'Could not confirm the address'),
        }),
      );
  }, [token, refresh]);

  const signedIn = !!getToken();
  return (
    <AuthShell title={state.kind === 'ok' ? 'Email confirmed' : state.kind === 'error' ? 'Link not valid' : 'Confirming your email'}>
      <div className="auth-form">
        {state.kind === 'busy' && <AuthLoading />}
        {state.kind === 'ok' && <p className="auth-body">{state.email} is confirmed. You can add sites now.</p>}
        {state.kind === 'error' && <p className="auth-body">{state.text}</p>}
        {state.kind !== 'busy' && (
          <Link className="btn btn-primary btn-block" to={signedIn ? '/sites' : '/login'}>
            {signedIn ? 'Go to your sites' : 'Sign in'}
          </Link>
        )}
      </div>
    </AuthShell>
  );
}

export function InvitePage() {
  const { token = '' } = useParams<{ token: string }>();
  const navigate = useNavigate();
  const { user, refresh } = useUser();
  const { config } = useBranding();
  const [info, setInfo] = useState<InviteInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const signedIn = !!getToken();

  useEffect(() => {
    let alive = true;
    fetchInviteInfo(token)
      .then((i) => alive && (i.valid ? setInfo(i) : setError('This invitation has expired or was already used. Ask for a new one.')))
      .catch(() => alive && setError('This invitation link is not valid. Check that you copied all of it.'));
    return () => {
      alive = false;
    };
  }, [token]);

  async function accept() {
    setBusy(true);
    setError(null);
    try {
      const r = await acceptInvite(token);
      writeViewAs(null);
      if (r.workspace?.id) writeWorkspaceId(r.workspace.id);
      await refresh();
      navigate('/sites');
    } catch (e) {
      setError(msg(e, 'Could not accept the invitation'));
    } finally {
      setBusy(false);
    }
  }

  if (!info && !error) {
    return (
      <AuthShell title="Workspace invitation">
        <AuthLoading />
      </AuthShell>
    );
  }

  if (!info) {
    return (
      <AuthShell title="Invitation not valid">
        <div className="auth-form">
          <p className="auth-body">{error}</p>
          <Link className="btn btn-block" to={signedIn ? '/sites' : '/login'}>
            {signedIn ? 'Go to your sites' : 'Sign in'}
          </Link>
        </div>
      </AuthShell>
    );
  }

  const mismatch = user && user.email.toLowerCase() !== info.email.toLowerCase() && user.role !== 'admin';
  const enc = encodeURIComponent(token);
  return (
    <AuthShell title={`Join ${info.workspace_name}`} lead={`You were invited to work in ${info.workspace_name}.`}>
      <div className="auth-form">
        <dl className="invite-facts">
          <div>
            <dt>Workspace</dt>
            <dd>{info.workspace_name}</dd>
          </div>
          <div>
            <dt>Your role</dt>
            <dd>{info.role}</dd>
          </div>
          <div>
            <dt>For</dt>
            <dd>{info.email}</dd>
          </div>
        </dl>
        {error && <div className="error-box">{error}</div>}
        {signedIn ? (
          <>
            {mismatch && (
              <div className="auth-note">
                You are signed in as {user?.email}. This invitation is for {info.email}; sign in with that address to accept it.
              </div>
            )}
            <button className="btn btn-primary btn-block" disabled={busy || !!mismatch} onClick={() => void accept()}>
              {busy ? 'Joining…' : `Accept and join ${info.workspace_name}`}
            </button>
          </>
        ) : (
          <>
            <Link className="btn btn-primary btn-block" to={`/login?invite=${enc}&email=${encodeURIComponent(info.email)}`}>
              Sign in to accept
            </Link>
            {config && config.registration_mode !== 'closed' && (
              <Link className="btn btn-block" to={`/register?invite=${enc}`}>
                Create an account
              </Link>
            )}
          </>
        )}
      </div>
    </AuthShell>
  );
}
