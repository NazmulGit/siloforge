import { useEffect, useState, type FormEvent } from 'react';
import { Link, Navigate, useNavigate, useSearchParams } from 'react-router-dom';
import { ApiError, getToken, register, setToken } from '../api';
import { useBranding } from '../branding';
import { fetchInviteInfo, registerV2, type Delivery, type InviteInfo } from '../saas';
import { useUser } from '../user';
import { writeWorkspaceId } from '../wsStore';
import { AuthLoading, AuthShell, PasswordField } from './auth/AuthShell';

function registerError(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 429) return 'Too many sign-ups from this network in the last hour. Try again later.';
    if (/domain/i.test(e.message)) return `${e.message}. Use an address on an allowed domain.`;
    if (e.status === 409 || /exists|taken|already/i.test(e.message)) return 'An account with this email already exists. Sign in instead.';
    return e.message;
  }
  return e instanceof Error ? e.message : 'Registration failed';
}

export function RegisterPage() {
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const inviteToken = params.get('invite') ?? '';
  const { setUser, refresh } = useUser();
  const { config, branding, reload } = useBranding();
  const [invite, setInvite] = useState<InviteInfo | null>(null);
  const [inviteErr, setInviteErr] = useState<string | null>(null);
  const [name, setName] = useState('');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [workspaceName, setWorkspaceName] = useState('');
  const [website, setWebsite] = useState(''); // honeypot: humans never see or fill it
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [checkEmail, setCheckEmail] = useState<{ email: string; delivery: Delivery } | null>(null);

  useEffect(() => {
    if (!inviteToken) return;
    let alive = true;
    fetchInviteInfo(inviteToken)
      .then((i) => {
        if (!alive) return;
        if (!i.valid) setInviteErr('This invitation has expired or was already used. Ask for a new one.');
        else {
          setInvite(i);
          setEmail(i.email);
        }
      })
      .catch(() => alive && setInviteErr('This invitation link is not valid. Check that you copied all of it.'));
    return () => {
      alive = false;
    };
  }, [inviteToken]);

  if (getToken() && !checkEmail) return <Navigate to={inviteToken ? `/invite/${encodeURIComponent(inviteToken)}` : '/sites'} replace />;

  const appName = branding.app_name.trim() || 'SiloForge';

  if (!config) {
    return (
      <AuthShell title="Create an account">
        <AuthLoading />
      </AuthShell>
    );
  }

  if (checkEmail) {
    return (
      <AuthShell title="Check your email">
        <div className="auth-form">
          <p className="auth-body">
            We sent a confirmation link to <b>{checkEmail.email}</b>. Open it to finish setting up. You can look around now, but
            adding sites waits until the address is confirmed.
          </p>
          {checkEmail.delivery === 'manual' && (
            <div className="auth-note">
              This server has no mail service yet, so the link was kept for your platform admin. Ask them to send it to you from
              Admin, Email.
            </div>
          )}
          <button className="btn btn-primary btn-block" onClick={() => navigate('/sites')}>
            Continue to {appName}
          </button>
        </div>
      </AuthShell>
    );
  }

  const mode = config.first_run ? 'open' : config.registration_mode;
  const support = branding.support_email.trim();

  if (mode === 'closed') {
    return (
      <AuthShell title="Registration is closed">
        <div className="auth-form">
          <p className="auth-body">
            New accounts on this server are created by the platform admin.{' '}
            {support ? (
              <>
                To get access, email <a href={`mailto:${support}`}>{support}</a>.
              </>
            ) : (
              'Ask them for an account or an invitation.'
            )}
          </p>
          <Link className="btn btn-block" to="/login">
            Back to sign in
          </Link>
        </div>
      </AuthShell>
    );
  }

  if (mode === 'invite_only' && (!inviteToken || inviteErr)) {
    return (
      <AuthShell title="You need an invitation">
        <div className="auth-form">
          <p className="auth-body">
            {inviteErr ?? 'Accounts on this server are by invitation only. Open the invitation link from your email to sign up.'}
          </p>
          <Link className="btn btn-block" to="/login">
            Back to sign in
          </Link>
        </div>
      </AuthShell>
    );
  }

  const canSubmit = email.trim() !== '' && password !== '' && confirm !== '' && !busy && !(inviteToken && !invite && !inviteErr);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (!canSubmit) return;
    if (password.length < 8) return setError('Use at least 8 characters for the password.');
    if (password !== confirm) return setError('The two passwords do not match.');
    setBusy(true);
    setError(null);
    try {
      if (config?.legacy) {
        const r = await register(email.trim(), password, name.trim() || undefined);
        setToken(r.token);
        setUser(r.user);
        navigate('/sites');
        return;
      }
      const r = await registerV2({
        email: email.trim(),
        password,
        name: name.trim() || undefined,
        invite_token: invite ? inviteToken : undefined,
        workspace_name: !invite && workspaceName.trim() ? workspaceName.trim() : undefined,
        website,
      });
      setToken(r.token);
      setUser(r.user);
      if (r.workspace?.id) writeWorkspaceId(r.workspace.id);
      void refresh();
      if (config?.first_run) void reload();
      if (r.email_verification_required) setCheckEmail({ email: r.user.email, delivery: r.verification_delivery ?? null });
      else navigate('/sites');
    } catch (err) {
      setError(registerError(err));
    } finally {
      setBusy(false);
    }
  }

  const title = config.first_run ? 'Create the admin account' : invite ? `Join ${invite.workspace_name}` : `Create your ${appName} account`;
  const lead = config.first_run
    ? 'This is the first account on this server. It becomes the platform admin and owns the Default workspace.'
    : invite
      ? `You were invited as ${invite.role === 'admin' ? 'an admin' : `a ${invite.role}`}. Set a password to join.`
      : undefined;

  return (
    <AuthShell title={title} lead={lead}>
      <form className="auth-form" onSubmit={(e) => void submit(e)} noValidate>
        {error && (
          <div className="error-box" role="alert">
            {error}
          </div>
        )}
        <label className="field auth-field">
          Name
          <input type="text" value={name} onChange={(e) => setName(e.target.value)} placeholder="Optional" autoComplete="name" autoFocus />
        </label>
        <label className="field auth-field">
          Email
          <input
            type="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            placeholder="you@example.com"
            autoComplete="email"
            readOnly={!!invite}
          />
          {invite && <span className="field-note faint">The invitation is for this address.</span>}
        </label>
        <PasswordField label="Password" value={password} onChange={setPassword} autoComplete="new-password" note="At least 8 characters." />
        <PasswordField label="Confirm password" value={confirm} onChange={setConfirm} autoComplete="new-password" />
        {mode === 'open' && !invite && !config.first_run && !config.legacy && (
          <label className="field auth-field">
            Workspace name
            <input
              type="text"
              value={workspaceName}
              onChange={(e) => setWorkspaceName(e.target.value)}
              placeholder="Optional, e.g. Acme Media"
              autoComplete="organization"
            />
            <span className="field-note faint">Your sites live in a workspace. You can rename it later.</span>
          </label>
        )}
        <div className="hp-field" aria-hidden="true">
          <label>
            Website
            <input type="text" tabIndex={-1} autoComplete="off" value={website} onChange={(e) => setWebsite(e.target.value)} />
          </label>
        </div>
        <button type="submit" className="btn btn-primary btn-block" disabled={!canSubmit}>
          {busy ? 'Creating account…' : config.first_run ? 'Create admin account' : 'Create account'}
        </button>
        {(branding.terms_url || branding.privacy_url) && !config.first_run && (
          <p className="field-note faint">By creating an account you accept the terms and privacy policy linked below.</p>
        )}
      </form>
      <div className="auth-switch">
        Already have an account? <Link to={inviteToken ? `/login?invite=${encodeURIComponent(inviteToken)}` : '/login'}>Sign in</Link>
      </div>
    </AuthShell>
  );
}
