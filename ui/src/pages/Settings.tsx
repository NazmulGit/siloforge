import { useCallback, useEffect, useMemo, useState, type FormEvent, type ReactNode } from 'react';
import { Link, Navigate, useSearchParams } from 'react-router-dom';
import {
  changePassword,
  disconnectGsc,
  fetchGscStatus,
  fetchHistory,
  fetchJobs,
  fetchUsage,
  saveGsc,
  ApiError,
  type GscStatus,
  type UsageSummary,
} from '../api';
import { AuditLogPanel } from '../components/AuditLogPanel';
import { BackupsPanel } from '../components/BackupsPanel';
import { LinkRulesPanel } from '../components/LinkRulesPanel';
import { NotificationsPanel } from '../components/NotificationsPanel';
import { ModelsPanel } from '../components/ModelsPanel';
import { PostLink } from '../components/PostLink';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { StatusChip } from '../components/StatusChip';
import { Toggle } from '../components/Toggle';
import { ApiMcpPanel } from '../components/settings/ApiMcpPanel';
import { BackupSettingsCard, LlmFallbackCard, ServerSettingsCard } from '../components/settings/ExtraCards';
import { ResourcesPanel } from '../components/settings/ResourcesPanel';
import { WordPressServerCards } from '../components/settings/WordPressPanel';
import { WpCredentialsForm } from '../components/WpCredentialsForm';
import { HelpButton } from '../docs';
import { readTheme, saveTheme, type Theme } from '../theme';
import { useActiveSite } from '../activeSite';
import { fmtDate, fmtDuration } from '../format';
import { useHints } from '../hints';
import { useSites } from '../sites';
import { useToast } from '../toast';
import { useUser } from '../user';
import type { HistoryRow, JobRun, SiteListItem } from '../types';

// ---------------------------------------------------------------------------
// Sub-nav definition
// ---------------------------------------------------------------------------

type SectionId =
  | 'general'
  | 'linking'
  | 'resources'
  | 'wordpress'
  | 'models'
  | 'api'
  | 'notifications'
  | 'backups'
  | 'activity'
  | 'account';

interface SectionDef {
  id: SectionId;
  label: string;
  hint: string;
  /** Sections only admins may see at all (not just read-only). */
  adminOnly?: boolean;
}

const SECTIONS: SectionDef[] = [
  { id: 'general', label: 'General', hint: 'Interface preferences, theme, usage totals and server options.' },
  { id: 'linking', label: 'Linking Rules', hint: 'Per-site link density limits and the post-injection verification switch.' },
  {
    id: 'resources',
    label: 'Resources & Performance',
    hint: 'CPU threads, concurrent jobs, memory caps and CPU/GPU balancing for embeddings. Admins only.',
    adminOnly: true,
  },
  { id: 'wordpress', label: 'WordPress', hint: 'Site credentials, defaults for new sites, crawling, Search Console and other CMSes.' },
  { id: 'models', label: 'AI Models', hint: 'LLM providers, models, fallback chains and the last-resort fallback. Admins only.', adminOnly: true },
  { id: 'api', label: 'API & MCP', hint: 'API tokens and their scopes, the MCP tool manager and how to connect Hermes Agent.' },
  { id: 'notifications', label: 'Notifications', hint: 'Where alerts go: webhook, ntfy, Telegram, Hermes, desktop toast. Admins only.', adminOnly: true },
  { id: 'backups', label: 'Backups', hint: 'Create, verify, download and schedule backups of the whole install. Admins only.', adminOnly: true },
  { id: 'activity', label: 'Activity', hint: 'Current jobs and the most recent applied links for the active site (plus the audit log for admins).' },
  { id: 'account', label: 'Account', hint: 'Your signed-in email and role, and change your password.' },
];

/** Section ids from older links, mapped onto the v2.1 sections. */
const LEGACY_SECTIONS: Record<string, SectionId> = {
  tokens: 'api',
  integrations: 'api',
  mcp: 'api',
  cms: 'wordpress',
  gsc: 'wordpress',
  plans: 'general',
};

// ---------------------------------------------------------------------------
// Shared bits
// ---------------------------------------------------------------------------

function PanelHead({ title, sub }: { title: string; sub: string }) {
  return (
    <div>
      <h2 style={{ fontSize: 17 }}>{title}</h2>
      <div className="page-sub" style={{ marginBottom: 14 }}>
        {sub}
      </div>
    </div>
  );
}

/** A dropdown for choosing which site the site-scoped panels apply to. */
function SitePicker({
  sites,
  siteId,
  onChange,
}: {
  sites: SiteListItem[];
  siteId: string;
  onChange: (id: string) => void;
}) {
  return (
    <label className="field settings-site-picker">
      Active site
      <select
        value={siteId}
        onChange={(e) => onChange(e.target.value)}
        data-hint="Choose which site these per-site settings apply to. Remembered on this device."
      >
        {sites.map((s) => (
          <option key={s.id} value={s.id}>
            {s.domain}
          </option>
        ))}
      </select>
    </label>
  );
}

/**
 * Wrapper for site-scoped panels. Prompts to add/pick a site when needed,
 * otherwise renders a site picker followed by the panel body.
 */
function SiteScope({
  siteId,
  sites,
  onChange,
  children,
}: {
  siteId: string | null;
  sites: SiteListItem[];
  onChange: (id: string) => void;
  children: (siteId: string) => ReactNode;
}) {
  if (sites.length === 0) {
    return (
      <div className="empty">
        <div className="empty-title">No sites yet</div>
        <div className="empty-hint">
          <Link to="/sites">Add a site</Link> first — this section needs a site to work with.
        </div>
      </div>
    );
  }
  if (!siteId) {
    return (
      <>
        <SitePicker sites={sites} siteId={sites[0]?.id ?? ''} onChange={onChange} />
        <div className="muted">Select a site to continue.</div>
      </>
    );
  }
  return (
    <>
      <SitePicker sites={sites} siteId={siteId} onChange={onChange} />
      {children(siteId)}
    </>
  );
}

// ---------------------------------------------------------------------------
// 1. General
// ---------------------------------------------------------------------------

function GeneralPanel() {
  const { enabled, setEnabled } = useHints();
  const [theme, setTheme] = useState<Theme>(readTheme);
  return (
    <div className="card">
      <PanelHead title="General" sub="Interface preferences for this device." />
      <div className="settings-list">
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Show help hints on hover</div>
            <div className="settings-row-desc">
              When on, hovering any button or control shows a short explanation of what it does.
            </div>
          </div>
          <Toggle
            checked={enabled}
            onChange={setEnabled}
            label="Show help hints on hover"
            dataHint="Turns the hover explanations on or off everywhere in the app. Your choice is remembered on this device."
          />
        </div>
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Theme</div>
            <div className="settings-row-desc">Light, dark, or follow this device&apos;s setting.</div>
          </div>
          <div className="segmented" role="radiogroup" aria-label="Theme">
            {(['system', 'light', 'dark'] as Theme[]).map((t) => (
              <button
                key={t}
                type="button"
                role="radio"
                aria-checked={theme === t}
                className={theme === t ? 'on' : ''}
                onClick={() => {
                  setTheme(t);
                  saveTheme(t);
                }}
              >
                {t === 'system' ? 'System' : t === 'light' ? 'Light' : 'Dark'}
              </button>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 2. Account
// ---------------------------------------------------------------------------

function AccountPanel() {
  const { user } = useUser();
  const toast = useToast();
  const [current, setCurrent] = useState('');
  const [next, setNext] = useState('');
  const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    if (next.length < 8) {
      setError('New password must be at least 8 characters.');
      return;
    }
    if (next !== confirm) {
      setError('New password and confirmation do not match.');
      return;
    }
    if (!current) {
      setError('Enter your current password.');
      return;
    }
    setBusy(true);
    try {
      await changePassword(current, next);
      toast.push('Password updated', 'success');
      setCurrent('');
      setNext('');
      setConfirm('');
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : err instanceof Error ? err.message : 'Could not update password';
      setError(msg);
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="card">
        <PanelHead title="Account" sub="Your identity in SiloForge." />
        <div className="settings-list">
          <div className="settings-row">
            <div className="settings-row-text">
              <div className="settings-row-label">Signed in as</div>
              <div className="settings-row-desc mono">{user?.email ?? '—'}</div>
            </div>
            {user && (
              <span
                className={`role-chip role-${user.role}`}
                data-hint="Your access level. Admins can manage every site; users are limited to their own."
              >
                {user.role}
              </span>
            )}
          </div>
        </div>
      </div>

      <div className="card">
        <div className="card-title">Change password</div>
        <form className="form-grid" onSubmit={(e) => void submit(e)}>
          {error && <div className="error-box">{error}</div>}
          <label className="field">
            Current password
            <input
              type="password"
              autoComplete="current-password"
              value={current}
              onChange={(e) => setCurrent(e.target.value)}
            />
          </label>
          <label className="field">
            New password
            <input
              type="password"
              autoComplete="new-password"
              value={next}
              onChange={(e) => setNext(e.target.value)}
            />
            <span className="faint" style={{ fontSize: 12 }}>
              At least 8 characters.
            </span>
          </label>
          <label className="field">
            Confirm new password
            <input
              type="password"
              autoComplete="new-password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
            />
          </label>
          <div className="form-actions">
            <button
              type="submit"
              className="btn btn-primary"
              disabled={busy}
              data-hint="Update your account password. You'll need your current password to confirm the change."
            >
              {busy ? 'Updating…' : 'Update password'}
            </button>
          </div>
        </form>
      </div>
    </>
  );
}

// ---------------------------------------------------------------------------
// 3. CMS Integration
// ---------------------------------------------------------------------------

function CmsPanel({ siteId, sites, onChange }: { siteId: string | null; sites: SiteListItem[]; onChange: (id: string) => void }) {
  return (
    <div>
      <PanelHead
        title="CMS Integration"
        sub="Feed content into SiloForge and place decided links back on any CMS."
      />
      <div className="connector-grid">
        {/* WordPress */}
        <div className="card connector">
          <div className="connector-head">
            <span className="connector-name">WordPress</span>
            <span className="connector-badge ok">Supported</span>
          </div>
          <p className="muted" style={{ margin: 0, fontSize: 13 }}>
            The primary connector — two-way sync via the <b>siloforge-connect</b> plugin.
          </p>
          <ol className="steps">
            <li>
              <Link to="/sites">Add your site</Link> on the Sites page.
            </li>
            <li>Copy the one-time API key it generates.</li>
            <li>Install the siloforge-connect plugin on your WordPress site.</li>
            <li>Paste the API key into the plugin settings.</li>
            <li>Run a Crawl to sync your posts, then a Plan.</li>
          </ol>
          <div className="faint" style={{ fontSize: 12.5 }}>
            Plugin zip: <code className="mono">D:\Code\siloforge-connect\releases\</code>
          </div>
          <div className="form-actions">
            <Link className="btn btn-primary btn-sm" to="/sites" data-hint="Open the Sites page to add a site and get its API key.">
              Add a site
            </Link>
          </div>
        </div>

        {/* Shopify */}
        <div className="card connector disabled">
          <div className="connector-head">
            <span className="connector-name">Shopify</span>
            <span className="connector-badge soon">Coming soon</span>
          </div>
          <p className="muted" style={{ margin: 0, fontSize: 13 }}>
            Native Shopify blog &amp; product internal-linking is on the roadmap.
          </p>
          <div className="form-actions">
            <button className="btn btn-sm" disabled data-hint="Shopify connector isn't available yet.">
              Connect
            </button>
          </div>
        </div>

        {/* Custom / CSV */}
        <div className="card connector">
          <div className="connector-head">
            <span className="connector-name">Custom / CSV</span>
            <span className="connector-badge ok">Supported</span>
          </div>
          <p className="muted" style={{ margin: 0, fontSize: 13 }}>
            Import content (<code className="mono">url,title,content_html</code>) from Webflow, Ghost or
            any CMS, and export decided links as CSV to place manually.
          </p>
          <div className="form-actions">
            {siteId ? (
              <Link
                className="btn btn-sm"
                to={`/site/${siteId}/integrations`}
                data-hint="Open the CSV import / export tools for the selected site."
              >
                Open CSV import / export
              </Link>
            ) : sites.length === 0 ? (
              <span className="faint" style={{ fontSize: 12.5 }}>
                <Link to="/sites">Add a site</Link> to use CSV import / export.
              </span>
            ) : (
              <span className="settings-inline-picker">
                <SitePicker sites={sites} siteId={sites[0]?.id ?? ''} onChange={onChange} />
              </span>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 4. Search Console (GSC)
// ---------------------------------------------------------------------------

function GscBody({ siteId }: { siteId: string }) {
  const toast = useToast();
  const [status, setStatus] = useState<GscStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creds, setCreds] = useState('');
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    setStatus(null);
    try {
      setStatus(await fetchGscStatus(siteId));
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load Search Console status');
    }
  }, [siteId]);

  useEffect(() => {
    void load();
  }, [load]);

  async function connect() {
    if (busy) return;
    if (!creds.trim()) {
      toast.push('Paste the service-account JSON first', 'error');
      return;
    }
    setBusy(true);
    try {
      const next = await saveGsc(siteId, creds.trim());
      setStatus(next);
      setCreds('');
      toast.push('Search Console connected', 'success');
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not connect Search Console', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function disconnect() {
    if (busy) return;
    setBusy(true);
    try {
      await disconnectGsc(siteId);
      setStatus({ connected: false, client_email: null, saved_at: null });
      toast.push('Search Console disconnected', 'info');
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not disconnect', 'error');
    } finally {
      setBusy(false);
    }
  }

  if (error) return <div className="error-box">{error}</div>;
  if (!status) return <Skeleton rows={5} />;

  return (
    <div className="card">
      <div className="card-title">Google Search Console</div>
      {status.connected ? (
        <div className="settings-list">
          <div className="settings-row">
            <div className="settings-row-text">
              <div className="settings-row-label">Connected</div>
              <div className="settings-row-desc mono">{status.client_email ?? 'service account'}</div>
              <div className="settings-row-desc faint">Saved {fmtDate(status.saved_at)}</div>
            </div>
            <button
              className="btn btn-danger btn-sm"
              disabled={busy}
              onClick={() => void disconnect()}
              data-hint="Remove the stored Search Console service-account credentials for this site."
            >
              Disconnect
            </button>
          </div>
        </div>
      ) : (
        <div className="form-grid" style={{ maxWidth: 620 }}>
          <p className="muted" style={{ margin: 0, fontSize: 13 }}>
            Paste the <b>service-account JSON</b> for a Google Cloud project with the Search Console API
            enabled. It's stored per site and used to weight suggestions by real search performance.
          </p>
          <textarea
            className="input gsc-textarea"
            placeholder='{ "type": "service_account", "client_email": "...", "private_key": "..." }'
            value={creds}
            onChange={(e) => setCreds(e.target.value)}
            spellCheck={false}
          />
          <div className="form-actions">
            <button
              className="btn btn-primary"
              disabled={busy}
              onClick={() => void connect()}
              data-hint="Save these Search Console credentials for the selected site and validate the connection."
            >
              {busy ? 'Connecting…' : 'Connect'}
            </button>
          </div>
        </div>
      )}
      <div className="info-note" style={{ marginTop: 14, marginBottom: 0 }}>
        <span className="info-note-icon" aria-hidden="true">
          ℹ
        </span>
        <span className="info-note-text">Rank-pull scoring activates on the next Plan for this site.</span>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 5. Plans & Usage
// ---------------------------------------------------------------------------

const USAGE_FIELDS: Array<{ key: keyof UsageSummary; label: string }> = [
  { key: 'sites', label: 'Sites' },
  { key: 'posts', label: 'Posts' },
  { key: 'paragraphs', label: 'Paragraphs' },
  { key: 'suggested', label: 'Suggested' },
  { key: 'approved', label: 'Approved' },
  { key: 'applied', label: 'Applied links' },
  { key: 'clusters', label: 'Clusters' },
];

function PlansPanel() {
  const [usage, setUsage] = useState<UsageSummary | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    fetchUsage()
      .then((u) => {
        if (alive) setUsage(u);
      })
      .catch((e: unknown) => {
        if (alive) setError(e instanceof Error ? e.message : 'Failed to load usage');
      });
    return () => {
      alive = false;
    };
  }, []);

  return (
    <div>
      <PanelHead title="Plans & Usage" sub="Your self-hosted footprint." />
      <div className="info-note" style={{ marginBottom: 14 }}>
        <span className="info-note-icon" aria-hidden="true">
          ★
        </span>
        <span className="info-note-text">
          <b>Self-hosted · unlimited</b> — no per-link cost, no credits, no seat limits.
        </span>
      </div>
      {error ? (
        <div className="error-box">{error}</div>
      ) : !usage ? (
        <Skeleton rows={4} />
      ) : (
        <div className="usage-grid">
          {USAGE_FIELDS.map((f) => (
            <div key={f.key} className="stat-card" data-hint={`Total ${f.label.toLowerCase()} across your self-hosted install.`}>
              <div className="stat-label">{f.label}</div>
              <div className="stat-value">{Number(usage[f.key] ?? 0).toLocaleString()}</div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 6. Activity
// ---------------------------------------------------------------------------

function ActivityBody({ siteId }: { siteId: string }) {
  const [jobs, setJobs] = useState<JobRun[] | null>(null);
  const [history, setHistory] = useState<HistoryRow[] | null>(null);

  useEffect(() => {
    let alive = true;
    let timer: number | undefined;
    const tick = async () => {
      try {
        const list = await fetchJobs(siteId);
        if (!alive) return;
        setJobs(list);
        // Live-poll every 4s while any job is queued or running.
        if (list.some((j) => j.status === 'queued' || j.status === 'running')) {
          timer = window.setTimeout(() => void tick(), 4000);
        }
      } catch {
        if (!alive) return;
        timer = window.setTimeout(() => void tick(), 4000);
      }
    };
    void tick();
    return () => {
      alive = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [siteId]);

  useEffect(() => {
    let alive = true;
    setHistory(null);
    fetchHistory(siteId)
      .then((r) => {
        if (alive) setHistory(r);
      })
      .catch(() => {
        if (alive) setHistory([]);
      });
    return () => {
      alive = false;
    };
  }, [siteId]);

  const active = (jobs ?? []).filter((j) => j.status === 'queued' || j.status === 'running');
  const recent = (history ?? []).slice(0, 10);

  return (
    <>
      <div className="card">
        <div className="card-title">Current jobs</div>
        {jobs === null ? (
          <Skeleton rows={3} />
        ) : active.length === 0 ? (
          <div className="muted">No active jobs. Trigger a Crawl or Plan from the Dashboard.</div>
        ) : (
          <table className="tbl">
            <thead>
              <tr>
                <th>Kind</th>
                <th>Status</th>
                <th>Started</th>
                <th>Duration</th>
              </tr>
            </thead>
            <tbody>
              {active.map((j) => (
                <tr key={j.id}>
                  <td className="mono">{j.kind}</td>
                  <td>
                    <StatusChip status={j.status} />
                  </td>
                  <td className="muted">{fmtDate(j.started_at ?? j.created_at)}</td>
                  <td className="muted">{fmtDuration(j.started_at, j.finished_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <div className="card">
        <div className="table-toolbar" style={{ marginBottom: 12 }}>
          <div className="card-title" style={{ marginBottom: 0 }}>
            Recent link history
          </div>
          <div className="spacer" />
          <Link
            className="btn btn-sm"
            to={`/site/${siteId}/history`}
            data-hint="Open the full applied/reverted link history for this site."
          >
            View full history
          </Link>
        </div>
        {history === null ? (
          <Skeleton rows={4} />
        ) : recent.length === 0 ? (
          <div className="muted">No links applied yet.</div>
        ) : (
          <table className="tbl">
            <thead>
              <tr>
                <th>Source → Target</th>
                <th>Status</th>
                <th>Applied</th>
              </tr>
            </thead>
            <tbody>
              {recent.map((r) => (
                <tr key={r.edge_id}>
                  <td className="pair-cell">
                    <div className="pair">
                      <PostLink className="src" url={r.source_url} title={r.source_title || '(untitled)'} />
                      <span className="arrow">→</span>
                      <PostLink className="dst" url={r.target_url} title={r.target_title || '(untitled)'} />
                    </div>
                  </td>
                  <td>
                    <StatusChip status={r.status} />
                  </td>
                  <td className="muted">{fmtDate(r.applied_at ?? r.decided_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </>
  );
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function SettingsPage() {
  const [params, setParams] = useSearchParams();
  const { sites } = useSites();
  const { user } = useUser();
  const { siteId, setSiteId } = useActiveSite(sites);

  const isAdmin = user?.role === 'admin';
  // Admin-only sections are removed from the nav AND from the set of reachable
  // ids, so a hand-typed ?section=models cannot open a panel the user may not see.
  const sections = useMemo(() => SECTIONS.filter((s) => !s.adminOnly || isAdmin), [isAdmin]);
  const sectionIds = useMemo(() => new Set<string>(sections.map((s) => s.id)), [sections]);

  const raw = params.get('section');
  const mapped = raw ? (LEGACY_SECTIONS[raw] ?? raw) : null;
  const section: SectionId = mapped && sectionIds.has(mapped) ? (mapped as SectionId) : 'general';

  const goTo = useCallback(
    (id: SectionId) => {
      setParams(id === 'general' ? {} : { section: id });
    },
    [setParams],
  );

  // No-flash effective site for scoped panels (defaults to first site).
  const effectiveSiteId = useMemo(() => siteId ?? sites[0]?.id ?? null, [siteId, sites]);
  const current = sections.find((s) => s.id === section);

  // The old Knowledge Base section now lives on the Docs page.
  if (raw === 'kb') return <Navigate to="/docs/operating-procedures" replace />;

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Settings</h1>
          <div className="page-sub">{current?.hint ?? 'Preferences, account, integrations and usage.'}</div>
        </div>
        <div className="spacer" />
        <HelpButton />
      </div>

      <div className="settings-hub">
        <nav className="settings-nav" aria-label="Settings sections">
          {sections.map((s) => (
            <button
              key={s.id}
              type="button"
              className={`settings-nav-item${section === s.id ? ' active' : ''}`}
              aria-current={section === s.id ? 'page' : undefined}
              data-hint={s.hint}
              onClick={() => goTo(s.id)}
            >
              {s.label}
            </button>
          ))}
        </nav>

        <div className="settings-panel">
          {section === 'general' && (
            <>
              <GeneralPanel />
              <PlansPanel />
              {isAdmin && <ServerSettingsCard />}
            </>
          )}
          {section === 'account' && <AccountPanel />}
          {section === 'linking' && (
            <>
              <PanelHead title="Linking Rules" sub="How many internal links a page may carry, and how carefully they're written." />
              <SiteScope siteId={effectiveSiteId} sites={sites} onChange={setSiteId}>
                {(id) => <LinkRulesPanel siteId={id} />}
              </SiteScope>
            </>
          )}
          {section === 'resources' && isAdmin && (
            <>
              <PanelHead title="Resources & Performance" sub="How much of this machine SiloForge may use, and where embeddings run." />
              <ResourcesPanel />
            </>
          )}
          {section === 'wordpress' && (
            <>
              <PanelHead title="WordPress" sub="How SiloForge connects to each site, and the defaults every site starts with." />
              <SiteScope siteId={effectiveSiteId} sites={sites} onChange={setSiteId}>
                {(id) => (
                  <>
                    <WpCredentialsForm siteId={id} />
                    <GscBody siteId={id} />
                  </>
                )}
              </SiteScope>
              {isAdmin && <WordPressServerCards />}
              <CmsPanel siteId={effectiveSiteId} sites={sites} onChange={setSiteId} />
            </>
          )}
          {section === 'models' && isAdmin && (
            <>
              <PanelHead title="AI Models" sub="Providers, the models under them, and the fallback chain each role runs on." />
              <ModelsPanel />
              <LlmFallbackCard />
            </>
          )}
          {section === 'activity' && (
            <>
              <PanelHead title="Activity" sub="Live jobs and the latest applied links for the active site." />
              <SiteScope siteId={effectiveSiteId} sites={sites} onChange={setSiteId}>
                {(id) => <ActivityBody siteId={id} />}
              </SiteScope>
              {isAdmin && <AuditLogPanel />}
              <div className="muted" style={{ fontSize: 13 }}>
                Every API, MCP and job event is on the <Link to="/logs">Logs</Link> page.
              </div>
            </>
          )}
          {section === 'notifications' && isAdmin && (
            <>
              <PanelHead title="Notifications" sub="Every alert shows under the bell in the top bar. Channels also deliver it elsewhere." />
              <NotificationsPanel />
            </>
          )}
          {section === 'backups' && isAdmin && (
            <>
              <PanelHead title="Backups" sub="Snapshots of the database, config and secrets. Download one before upgrades or moves." />
              <BackupsPanel />
              <BackupSettingsCard />
            </>
          )}
          {section === 'api' && (
            <>
              <PanelHead title="API & MCP" sub="Tokens for Hermes, MCP clients and scripts, which MCP tools they may use, and how to connect." />
              <ApiMcpPanel />
            </>
          )}
        </div>
      </div>
    </div>
  );
}
