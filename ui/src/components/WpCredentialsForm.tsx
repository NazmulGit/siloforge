// Per-site WordPress connection: username + application password (write-only) and the
// post-inject verify switch. Saved through the v1 PATCH /sites/:id {settings} merge; the
// server moves credentials into encrypted storage and only reports has_wp_credentials + wp_user.

import { useCallback, useEffect, useState } from 'react';
import { fetchSite, patchSiteSettings } from '../api';
import { useToast } from '../toast';
import type { Site } from '../types';
import { Skeleton } from './Skeleton';
import { Toggle } from './Toggle';

export function WpCredentialsForm({ siteId, compact }: { siteId: string; compact?: boolean }) {
  const toast = useToast();
  const [site, setSite] = useState<Site | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [user, setUser] = useState('');
  const [pass, setPass] = useState('');
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      const s = await fetchSite(siteId);
      setSite(s);
      setUser(s.wp_user ?? '');
      setPass('');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load the site');
    }
  }, [siteId]);

  useEffect(() => {
    setSite(null);
    void load();
  }, [load]);

  async function saveCreds() {
    if (!user.trim()) {
      toast.push('Enter the WordPress username', 'error');
      return;
    }
    if (!pass.trim() && !site?.has_wp_credentials) {
      toast.push('Enter the application password', 'error');
      return;
    }
    setBusy(true);
    try {
      await patchSiteSettings(siteId, { wp_user: user.trim(), ...(pass.trim() ? { wp_app_password: pass.trim() } : {}) });
      toast.push('WordPress credentials saved', 'success');
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not save credentials', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function setVerify(verify: boolean) {
    setBusy(true);
    try {
      await patchSiteSettings(siteId, { skip_inject_verify: !verify });
      toast.push(verify ? 'Links are verified after injection' : 'Post-injection verify turned off', 'info');
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not save', 'error');
    } finally {
      setBusy(false);
    }
  }

  if (error) return <div className="error-box">{error}</div>;
  if (!site) return <Skeleton rows={3} />;
  const verify = site.settings?.skip_inject_verify !== true;

  return (
    <div className={compact ? '' : 'card'}>
      {!compact && <div className="card-title">WordPress connection for {site.domain}</div>}
      <div className="cred-status">
        <span className={`chip ${site.has_wp_credentials ? 'chip-done' : 'chip-queued'}`}>
          {site.has_wp_credentials ? 'credentials saved' : 'no credentials'}
        </span>
        {site.has_wp_credentials && site.wp_user && (
          <span className="muted">
            as <b>{site.wp_user}</b>
          </span>
        )}
        {site.has_wp_credentials === undefined && <span className="faint">This server does not report credential status.</span>}
      </div>
      <p className="muted cred-help">
        SiloForge writes links through the WordPress REST API as this user. Create an application password in WordPress under
        Users, Profile, Application Passwords. The password is stored encrypted and never shown again.
      </p>
      <div className="cred-grid">
        <label className="field">
          WordPress username
          <input className="input" autoComplete="off" value={user} onChange={(e) => setUser(e.target.value)} placeholder="editor" />
        </label>
        <label className="field">
          Application password
          <input
            className="input"
            type="password"
            autoComplete="new-password"
            value={pass}
            onChange={(e) => setPass(e.target.value)}
            placeholder={site.has_wp_credentials ? 'saved; type to replace' : 'abcd efgh ijkl mnop qrst uvwx'}
          />
        </label>
        <div className="form-actions">
          <button className="btn btn-primary btn-sm" disabled={busy} onClick={() => void saveCreds()}>
            {busy ? 'Saving…' : 'Save credentials'}
          </button>
        </div>
      </div>
      <div className="settings-list">
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Verify links after injection</div>
            <div className="settings-row-desc">Re-reads each changed post to confirm the link is live. Recommended; turn off only for very slow sites.</div>
          </div>
          <Toggle checked={verify} onChange={(v) => void setVerify(v)} disabled={busy} label="Verify links after injection" />
        </div>
      </div>
    </div>
  );
}
