import { useState, type FormEvent } from 'react';
import { Link, Navigate, useNavigate, useSearchParams } from 'react-router-dom';
import { ApiError, getToken, loginTwoFactor, loginV2, needsTwoFactor, setToken, type AuthResponse } from '../api';
import { useBranding } from '../branding';
import { useUser } from '../user';
import { AuthLoading, AuthShell, PasswordField } from './auth/AuthShell';

function loginError(e: unknown, support: string): string {
  if (e instanceof ApiError) {
    const contact = support ? ` Contact ${support}.` : ' Ask your platform admin.';
    // The server's lockout text usually says how long to wait; keep it when it does.
    if (e.status === 429) return /\d/.test(e.message) ? e.message : 'Too many failed sign-ins. Wait a few minutes, then try again.';
    if (e.status === 403 && /disabled/i.test(e.message)) return `This account is disabled.${contact}`;
    if (e.status === 401 && /two-factor/i.test(e.message)) return e.message;
    if (e.status === 401) return 'That email and password do not match an account.';
    if (e.status === 0) return e.message;
    return e.message;
  }
  return e instanceof Error ? e.message : 'Sign-in failed';
}

export function LoginPage() {
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const invite = params.get('invite');
  const { setUser, refresh } = useUser();
  const { config, branding } = useBranding();
  const [email, setEmail] = useState(params.get('email') ?? '');
  const [password, setPassword] = useState('');
  const [remember, setRemember] = useState(true);
  const [challenge, setChallenge] = useState<string | null>(null);
  const [code, setCode] = useState('');
  const [recovery, setRecovery] = useState(false);
  const [forgotHint, setForgotHint] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const next = invite ? `/invite/${encodeURIComponent(invite)}` : '/sites';

  if (getToken()) return <Navigate to={next} replace />;

  const support = branding.support_email.trim();
  const appName = branding.app_name.trim() || 'SiloForge';

  async function finish(res: AuthResponse) {
    setToken(res.token);
    setUser(res.user);
    await refresh();
    navigate(next);
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (busy || !email.trim() || !password) return;
    setBusy(true);
    setError(null);
    try {
      const res = await loginV2(email.trim(), password, remember);
      if (needsTwoFactor(res)) {
        setChallenge(res.challenge);
        setCode('');
      } else await finish(res);
    } catch (err) {
      setError(loginError(err, support));
    } finally {
      setBusy(false);
    }
  }

  async function submitCode(e: FormEvent) {
    e.preventDefault();
    if (busy || !challenge || !code.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await finish(await loginTwoFactor(challenge, code.trim()));
    } catch (err) {
      if (err instanceof ApiError && err.status === 401 && /challenge|expired/i.test(err.message)) {
        setChallenge(null);
        setError('The sign-in step timed out. Enter your password again.');
      } else if (err instanceof ApiError && err.status === 401) {
        setError(recovery ? 'That recovery code is not valid or was already used.' : 'That code is not valid. Check the time on your phone and try the newest code.');
      } else setError(loginError(err, support));
    } finally {
      setBusy(false);
    }
  }

  if (!config) {
    return (
      <AuthShell title="Sign in">
        <AuthLoading />
      </AuthShell>
    );
  }

  if (challenge) {
    return (
      <AuthShell
        title="Two-step check"
        lead={recovery ? 'Enter one of the recovery codes you saved when you turned on two-factor sign-in.' : 'Enter the 6-digit code from your authenticator app.'}
      >
        <form className="auth-form" onSubmit={(e) => void submitCode(e)}>
          {error && <div className="error-box">{error}</div>}
          <label className="field auth-field">
            {recovery ? 'Recovery code' : 'Authentication code'}
            <input
              key={recovery ? 'rc' : 'totp'}
              className={recovery ? 'code-input' : 'code-input otp'}
              type="text"
              inputMode={recovery ? 'text' : 'numeric'}
              autoComplete="one-time-code"
              pattern={recovery ? undefined : '[0-9 ]*'}
              maxLength={recovery ? 32 : 7}
              value={code}
              onChange={(e) => setCode(recovery ? e.target.value : e.target.value.replace(/[^0-9]/g, ''))}
              placeholder={recovery ? 'xxxxx-xxxxx' : '123456'}
              autoFocus
            />
          </label>
          <button type="submit" className="btn btn-primary btn-block" disabled={busy || (!recovery && code.length !== 6) || !code}>
            {busy ? 'Checking…' : 'Verify and sign in'}
          </button>
          <div className="auth-links">
            <button
              type="button"
              className="link-btn"
              onClick={() => {
                setRecovery((v) => !v);
                setCode('');
                setError(null);
              }}
            >
              {recovery ? 'Use the authenticator app' : 'Use a recovery code'}
            </button>
            <button
              type="button"
              className="link-btn"
              onClick={() => {
                setChallenge(null);
                setPassword('');
                setError(null);
              }}
            >
              Back
            </button>
          </div>
        </form>
      </AuthShell>
    );
  }

  const canRegister = config.first_run || config.registration_mode !== 'closed';
  const registerHref = invite ? `/register?invite=${encodeURIComponent(invite)}` : '/register';

  return (
    <AuthShell title={`Sign in to ${appName}`} lead={invite ? 'Sign in to accept your workspace invitation.' : undefined}>
      {config.first_run && (
        <div className="auth-callout">
          <b>No accounts yet.</b> This server is new. <Link to="/register">Create the admin account</Link> to get started.
        </div>
      )}
      <form className="auth-form" onSubmit={(e) => void submit(e)}>
        {error && (
          <div className="error-box" role="alert">
            {error}
          </div>
        )}
        <label className="field auth-field">
          Email
          <input
            type="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            placeholder="you@example.com"
            autoComplete="username"
            autoFocus
          />
        </label>
        <PasswordField label="Password" value={password} onChange={setPassword} autoComplete="current-password" />
        <div className="auth-row">
          <label className="check-line">
            <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} />
            Keep me signed in for 30 days
          </label>
          {config.email_enabled ? (
            <Link to="/forgot">Forgot password?</Link>
          ) : (
            <button type="button" className="link-btn" aria-expanded={forgotHint} onClick={() => setForgotHint((v) => !v)}>
              Forgot password?
            </button>
          )}
        </div>
        {forgotHint && !config.email_enabled && (
          <div className="auth-note">
            This server does not send email yet. Ask your platform admin{support ? ` (${support})` : ''} for a reset link; they can
            create one from Admin, Users.
          </div>
        )}
        <button type="submit" className="btn btn-primary btn-block" disabled={busy || !email.trim() || !password}>
          {busy ? 'Signing in…' : 'Sign in'}
        </button>
      </form>
      {canRegister && !config.first_run && (
        <div className="auth-switch">
          {config.registration_mode === 'invite_only' && !invite ? (
            <>Accounts are by invitation. Have one? Open the link from your email.</>
          ) : (
            <>
              No account yet? <Link to={registerHref}>Create an account</Link>
            </>
          )}
        </div>
      )}
    </AuthShell>
  );
}
