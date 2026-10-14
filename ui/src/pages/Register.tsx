import { useState, type FormEvent } from 'react';
import { Link, Navigate, useNavigate } from 'react-router-dom';
import { getToken, register, setToken } from '../api';
import { useUser } from '../user';

export function RegisterPage() {
  const navigate = useNavigate();
  const { setUser } = useUser();
  const [name, setName] = useState('');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  if (getToken()) return <Navigate to="/sites" replace />;

  const canSubmit = email.trim() !== '' && password !== '' && confirm !== '' && !busy;

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (!canSubmit) return;
    if (password !== confirm) {
      setError('Passwords do not match');
      return;
    }
    if (password.length < 8) {
      setError('Password must be at least 8 characters');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const { token, user } = await register(email.trim(), password, name.trim() || undefined);
      setToken(token);
      setUser(user);
      navigate('/sites');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Registration failed');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="login-wrap">
      <form className="login-card" onSubmit={(e) => void submit(e)}>
        <div className="login-logo">
          <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="var(--ember)" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
            <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
            <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
          </svg>
          <span>
            SILO<b>FORGE</b>
          </span>
        </div>
        <div className="login-sub">Create your account</div>
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          Name
          <input
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="Your name (optional)"
            autoComplete="name"
            autoFocus
          />
        </label>
        <label className="field">
          Email
          <input
            type="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            placeholder="you@example.com"
            autoComplete="email"
          />
        </label>
        <label className="field">
          Password
          <input
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            placeholder="At least 8 characters…"
            autoComplete="new-password"
          />
        </label>
        <label className="field">
          Confirm password
          <input
            type="password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            placeholder="Repeat the password…"
            autoComplete="new-password"
          />
        </label>
        <button type="submit" className="btn btn-primary" disabled={!canSubmit}>
          {busy ? 'Creating account…' : 'Create account'}
        </button>
        <div className="login-sub" style={{ marginTop: 12 }}>
          Already have an account? <Link to="/login">Sign in</Link>
        </div>
      </form>
    </div>
  );
}
