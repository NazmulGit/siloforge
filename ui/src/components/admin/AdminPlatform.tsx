// Admin → Registration, Branding and Deployment: sections of GET/PUT /api/v2/admin/platform.

import { useCallback, useEffect, useState, type CSSProperties } from 'react';
import { Link } from 'react-router-dom';
import { safeColor, safeUrl, useBranding, Wordmark } from '../../branding';
import { fetchPlans, fetchPlatform, putPlatform, type Branding, type Plan, type PlatformSettings } from '../../saas';
import { useToast } from '../../toast';
import { Skeleton } from '../Skeleton';
import { Toggle } from '../Toggle';
import { errText } from './common';

function usePlatform() {
  const [data, setData] = useState<PlatformSettings | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(async () => {
    try {
      setData(await fetchPlatform());
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load platform settings'));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);
  return { data, error, load, setData };
}

function domains(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((d) => d.trim().toLowerCase().replace(/^@/, ''))
    .filter(Boolean);
}

export function AdminRegistration() {
  const toast = useToast();
  const { reload: reloadConfig } = useBranding();
  const { data, error, load } = usePlatform();
  const [reg, setReg] = useState<PlatformSettings['registration'] | null>(null);
  const [allow, setAllow] = useState('');
  const [deny, setDeny] = useState('');
  const [plans, setPlans] = useState<Plan[]>([]);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!data) return;
    setReg(data.registration);
    setAllow((data.registration.allowed_domains ?? []).join('\n'));
    setDeny((data.registration.denied_domains ?? []).join('\n'));
  }, [data]);
  useEffect(() => {
    fetchPlans()
      .then(setPlans)
      .catch(() => setPlans([]));
  }, []);

  if (error) return <div className="error-box">{error}</div>;
  if (!reg) return <Skeleton rows={6} />;

  async function save() {
    if (!reg) return;
    setBusy(true);
    try {
      await putPlatform({ registration: { ...reg, allowed_domains: domains(allow), denied_domains: domains(deny) } });
      toast.push('Registration settings saved', 'success');
      await load();
      void reloadConfig();
    } catch (e) {
      toast.push(errText(e, 'Could not save'), 'error');
    } finally {
      setBusy(false);
    }
  }

  const MODES: Array<{ id: PlatformSettings['registration']['mode']; label: string; desc: string }> = [
    { id: 'closed', label: 'Closed', desc: 'Only platform admins create accounts. The safe default for a private install.' },
    { id: 'invite_only', label: 'Invite only', desc: 'People sign up from an invitation link sent by a workspace admin.' },
    { id: 'open', label: 'Open', desc: `Anyone can sign up and gets their own workspace on the default plan.` },
  ];

  return (
    <div className="card">
      <div className="card-title">Who can sign up</div>
      <div className="mode-pick" role="radiogroup" aria-label="Registration mode">
        {MODES.map((m) => (
          <label key={m.id} className={`mode-opt${reg.mode === m.id ? ' on' : ''}`}>
            <input type="radio" name="reg-mode" checked={reg.mode === m.id} onChange={() => setReg({ ...reg, mode: m.id })} />
            <span className="mode-opt-label">{m.label}</span>
            <span className="mode-opt-desc">{m.desc}</span>
          </label>
        ))}
      </div>
      <div className="settings-list">
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Require email verification</div>
            <div className="settings-row-desc">New users can sign in at once, but cannot add sites until they open the emailed link.</div>
          </div>
          <Toggle checked={reg.require_email_verification} onChange={(v) => setReg({ ...reg, require_email_verification: v })} label="Require email verification" />
        </div>
      </div>
      <div className="form-cols">
        <label className="field">
          Allowed email domains
          <textarea rows={4} value={allow} onChange={(e) => setAllow(e.target.value)} placeholder={'example.com\nagency.co'} />
          <span className="field-note faint">One per line. Empty allows every domain.</span>
        </label>
        <label className="field">
          Blocked email domains
          <textarea rows={4} value={deny} onChange={(e) => setDeny(e.target.value)} placeholder="mailinator.com" />
          <span className="field-note faint">One per line. Checked after the allowed list.</span>
        </label>
        <label className="field">
          Sign-ups per IP address per hour
          <input
            type="number"
            min={1}
            value={reg.rate_limit_per_hour}
            onChange={(e) => setReg({ ...reg, rate_limit_per_hour: Math.max(1, Math.floor(Number(e.target.value) || 1)) })}
          />
        </label>
        <label className="field">
          Plan for new workspaces
          <select value={reg.default_plan_id} onChange={(e) => setReg({ ...reg, default_plan_id: e.target.value })}>
            {!plans.some((p) => p.id === reg.default_plan_id) && <option value={reg.default_plan_id}>{reg.default_plan_id}</option>}
            {plans.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </label>
      </div>
      {reg.mode !== 'closed' && (
        <div className="info-note">
          <span className="info-note-text">
            Open or invite sign-up on a public address: set up <Link to="/admin?tab=email">email</Link>, use HTTPS (see{' '}
            <Link to="/admin?tab=deployment">Deployment</Link>) and turn on two-factor sign-in for every admin.
          </span>
        </div>
      )}
      <div className="form-actions">
        <button className="btn btn-primary" disabled={busy} onClick={() => void save()}>
          {busy ? 'Saving…' : 'Save registration settings'}
        </button>
      </div>
    </div>
  );
}

function BrandPreview({ b }: { b: Branding }) {
  const color = safeColor(b.primary_color);
  const logo = safeUrl(b.logo_url);
  const name = b.app_name.trim() || 'SiloForge';
  return (
    <div className="brand-preview" style={color ? ({ '--brand': color } as CSSProperties) : undefined} aria-label="Sign-in page preview">
      <div className="brand-preview-art">
        {logo ? <img src={logo} alt="" /> : <span className="brand-preview-dot" />}
        <span className="auth-wordmark">
          <Wordmark name={name} />
        </span>
      </div>
      <div className="brand-preview-form">
        <div className="brand-preview-title">Sign in to {name}</div>
        <div className="brand-preview-input" />
        <div className="brand-preview-input" />
        <div className="brand-preview-btn">Sign in</div>
        <div className="brand-preview-legal">
          {b.terms_url && <span>Terms</span>}
          {b.privacy_url && <span>Privacy</span>}
          {b.support_email && <span>{b.support_email}</span>}
        </div>
      </div>
    </div>
  );
}

export function AdminBranding() {
  const toast = useToast();
  const { reload: reloadConfig, preview } = useBranding();
  const { data, error, load } = usePlatform();
  const [b, setB] = useState<Branding | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (data) setB(data.branding);
  }, [data]);
  // Leaving the tab drops any unsaved live preview.
  useEffect(() => () => preview(null), [preview]);

  if (error) return <div className="error-box">{error}</div>;
  if (!b) return <Skeleton rows={6} />;

  const colorOk = !b.primary_color || safeColor(b.primary_color) !== null;
  const set = (k: keyof Branding, v: string) => setB({ ...b, [k]: v });

  async function save() {
    if (!b) return;
    setBusy(true);
    try {
      await putPlatform({ branding: b });
      toast.push('Branding saved', 'success');
      preview(null);
      await load();
      await reloadConfig();
    } catch (e) {
      toast.push(errText(e, 'Could not save'), 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="card">
      <div className="brand-grid">
        <div className="form-grid" style={{ maxWidth: 'none' }}>
          <label className="field">
            App name
            <input value={b.app_name} onChange={(e) => set('app_name', e.target.value)} placeholder="SiloForge" />
          </label>
          <label className="field">
            Logo URL
            <input value={b.logo_url} onChange={(e) => set('logo_url', e.target.value)} placeholder="https://example.com/logo.svg" />
            <span className="field-note faint">Square image, shown at 28 px. HTTPS recommended.</span>
          </label>
          <label className="field">
            Primary colour
            <span className="color-row">
              <input type="color" aria-label="Pick a colour" value={safeColor(b.primary_color) ?? '#2a5bd7'} onChange={(e) => set('primary_color', e.target.value)} />
              <input className="mono" value={b.primary_color} onChange={(e) => set('primary_color', e.target.value)} placeholder="#2a5bd7" />
            </span>
            {!colorOk && <span className="field-note tone-bad">Use a hex colour such as #2a5bd7.</span>}
          </label>
          <label className="field">
            Support email
            <input type="email" value={b.support_email} onChange={(e) => set('support_email', e.target.value)} placeholder="support@example.com" />
            <span className="field-note faint">Shown on the sign-in pages and in upgrade prompts.</span>
          </label>
          <label className="field">
            Terms URL
            <input value={b.terms_url} onChange={(e) => set('terms_url', e.target.value)} placeholder="https://example.com/terms" />
          </label>
          <label className="field">
            Privacy URL
            <input value={b.privacy_url} onChange={(e) => set('privacy_url', e.target.value)} placeholder="https://example.com/privacy" />
          </label>
          <div className="form-actions">
            <button className="btn" onClick={() => preview(b)} data-hint="Applies these values to this browser tab only, so you can look around before saving.">
              Preview in app
            </button>
            <button className="btn btn-primary" disabled={busy || !colorOk} onClick={() => void save()}>
              {busy ? 'Saving…' : 'Save branding'}
            </button>
          </div>
        </div>
        <BrandPreview b={b} />
      </div>
    </div>
  );
}

export function AdminDeployment() {
  const toast = useToast();
  const { data, error, load } = usePlatform();
  const [dep, setDep] = useState<PlatformSettings['deployment'] | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (data) setDep(data.deployment);
  }, [data]);

  if (error) return <div className="error-box">{error}</div>;
  if (!dep) return <Skeleton rows={4} />;

  const url = dep.public_url.trim();
  const insecure = url.startsWith('http://') && !/^http:\/\/(127\.0\.0\.1|localhost)(:|\/|$)/.test(url);

  async function save() {
    if (!dep) return;
    setBusy(true);
    try {
      await putPlatform({ deployment: { public_url: dep.public_url.trim().replace(/\/+$/, ''), trust_proxy: dep.trust_proxy } });
      toast.push('Deployment settings saved', 'success');
      await load();
    } catch (e) {
      toast.push(errText(e, 'Could not save'), 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="card">
      <div className="form-grid" style={{ maxWidth: 640 }}>
        <label className="field">
          Public URL
          <input className="mono" value={dep.public_url} onChange={(e) => setDep({ ...dep, public_url: e.target.value })} placeholder="https://links.example.com" />
          <span className="field-note faint">The address people use to reach this server. Links in emails (verify, reset, invite) start with it.</span>
          {insecure && <span className="field-note tone-bad">Plain HTTP on a public address sends passwords unencrypted. Put a TLS proxy in front.</span>}
        </label>
      </div>
      <div className="settings-list">
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Behind a reverse proxy</div>
            <div className="settings-row-desc">
              Trust X-Forwarded-For and X-Forwarded-Proto from Caddy, nginx or Cloudflare Tunnel, so rate limits and logs see the real
              visitor address. Leave off when the server is reached directly.
            </div>
          </div>
          <Toggle checked={dep.trust_proxy} onChange={(v) => setDep({ ...dep, trust_proxy: v })} label="Trust proxy headers" />
        </div>
      </div>
      <div className="info-note">
        <span className="info-note-text">
          Server addresses (binds) are set in siloforge.toml or SILOFORGE_BINDS and need a restart. The step-by-step guide with Caddy and
          Cloudflare Tunnel examples and a security checklist is in <Link to="/docs/saas-deployment">SaaS deployment</Link>.
        </span>
      </div>
      <div className="form-actions">
        <button className="btn btn-primary" disabled={busy} onClick={() => void save()}>
          {busy ? 'Saving…' : 'Save deployment settings'}
        </button>
      </div>
    </div>
  );
}
