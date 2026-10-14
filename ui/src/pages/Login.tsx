import { useState, type FormEvent } from 'react';
import { Link, Navigate, useNavigate } from 'react-router-dom';
import { getToken, login, setToken } from '../api';
import { useUser } from '../user';

export function LoginPage() {
  const navigate = useNavigate();
  const { setUser } = useUser();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  if (getToken()) return <Navigate to="/sites" replace />;

  const canSubmit = email.trim() !== '' && password !== '' && !busy;

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      const { token, user } = await login(email.trim(), password);
      setToken(token);
      setUser(user);
      navigate('/sites');
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Login failed');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="login-wrap">
      <form className="login-card" onSubmit={(e) => void submit(e)}>
        <div className="login-logo">
          <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#2f6bff" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
            <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
            <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
          </svg>
          <span>
            SILO<b>FORGE</b>
          </span>
        </div>
        <div className="login-sub">Internal linking control room</div>
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          Email
          <input
            type="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            placeholder="you@example.com"
            autoComplete="email"
            autoFocus
          />
        </label>
        <label className="field">
          Password
          <input
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            placeholder="Your password…"
            autoComplete="current-password"
          />
        </label>
        <button type="submit" className="btn btn-primary" disabled={!canSubmit}>
          {busy ? 'Signing in…' : 'Sign in'}
        </button>
        <div className="login-sub" style={{ marginTop: 12 }}>
          No account yet? <Link to="/register">Create an account</Link>
        </div>
      </form>
    </div>
  );
}
