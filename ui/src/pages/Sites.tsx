import { useState, type FormEvent } from 'react';
import { useNavigate } from 'react-router-dom';
import { createSite, triggerCrawl, triggerPlan } from '../api';
import { Modal } from '../components/Modal';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { fmtDate } from '../format';
import { useJobTracker } from '../hooks';
import { useSites } from '../sites';
import { useToast } from '../toast';
import {
  EDGE_STATUSES,
  siteEdgeCounts,
  sitePostCount,
  type Site,
  type SiteListItem,
  type SiteMode,
} from '../types';

function EdgeMiniCounts({ site }: { site: SiteListItem }) {
  const counts = siteEdgeCounts(site);
  const entries = EDGE_STATUSES.filter((s) => (counts[s] ?? 0) > 0);
  if (entries.length === 0) return <span className="faint">—</span>;
  return (
    <div className="edge-mini-counts">
      {entries.map((s) => (
        <span key={s} className={`chip chip-${s}`} title={s}>
          {counts[s]} {s}
        </span>
      ))}
    </div>
  );
}

export function SitesPage() {
  const { sites, loading, error, reload } = useSites();
  const navigate = useNavigate();
  const toast = useToast();
  const track = useJobTracker();

  const [showAdd, setShowAdd] = useState(false);
  const [domain, setDomain] = useState('');
  const [wpUrl, setWpUrl] = useState('');
  const [mode, setMode] = useState<SiteMode>('wp_rest');
  const [saving, setSaving] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);
  const [createdSite, setCreatedSite] = useState<Site | null>(null);
  const [copied, setCopied] = useState(false);
  const [busyRow, setBusyRow] = useState<string | null>(null);

  async function submitAdd(e: FormEvent) {
    e.preventDefault();
    if (!domain.trim() || saving) return;
    setSaving(true);
    setAddError(null);
    try {
      const body: { domain: string; wp_url?: string; mode?: SiteMode } = { domain: domain.trim(), mode };
      if (wpUrl.trim()) body.wp_url = wpUrl.trim();
      const site = await createSite(body);
      setShowAdd(false);
      setDomain('');
      setWpUrl('');
      setMode('wp_rest');
      setCreatedSite(site);
      setCopied(false);
      void reload();
    } catch (err) {
      setAddError(err instanceof Error ? err.message : 'Failed to create site');
    } finally {
      setSaving(false);
    }
  }

  async function copyKey(key: string) {
    try {
      await navigator.clipboard.writeText(key);
      setCopied(true);
    } catch {
      toast.push('Copy failed — select the key manually', 'error');
    }
  }

  async function runCrawl(site: SiteListItem) {
    setBusyRow(site.id);
    try {
      const { job_run_id } = await triggerCrawl(site.id);
      track(site.id, job_run_id, `Crawl ${site.domain}`, () => void reload());
    } catch (err) {
      toast.push(err instanceof Error ? err.message : 'Crawl failed to start', 'error');
    } finally {
      setBusyRow(null);
    }
  }

  async function runPlan(site: SiteListItem) {
    setBusyRow(site.id);
    try {
      const { job_run_id } = await triggerPlan(site.id);
      track(site.id, job_run_id, `Plan ${site.domain}`, () => void reload());
    } catch (err) {
      toast.push(err instanceof Error ? err.message : 'Plan failed to start', 'error');
    } finally {
      setBusyRow(null);
    }
  }

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Sites</h1>
          <div className="page-sub">Connected WordPress sites and their link inventory.</div>
        </div>
        <div className="spacer" />
        <button
          className="btn btn-primary"
          onClick={() => setShowAdd(true)}
          data-hint="Connect a new WordPress or sitemap site. You'll get a one-time API key for the SiloForge Connect plugin."
        >
          + Add Site
        </button>
      </div>

      {error && <div className="error-box" style={{ marginBottom: 14 }}>{error}</div>}

      {loading ? (
        <Skeleton rows={6} />
      ) : sites.length === 0 ? (
        <EmptyState title="No sites yet" hint="Add your first site to start crawling and planning internal links." />
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Domain</th>
                <th>Mode</th>
                <th className="num">Posts</th>
                <th>Edges</th>
                <th>Last sync</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {sites.map((s) => (
                <tr key={s.id}>
                  <td>
                    <a
                      href={`/site/${s.id}/dashboard`}
                      onClick={(e) => {
                        e.preventDefault();
                        navigate(`/site/${s.id}/dashboard`);
                      }}
                    >
                      {s.domain}
                    </a>
                  </td>
                  <td>
                    <span className="chip chip-mode">{s.mode}</span>
                  </td>
                  <td className="num">{sitePostCount(s)}</td>
                  <td>
                    <EdgeMiniCounts site={s} />
                  </td>
                  <td className="muted">{fmtDate(s.last_sync_at)}</td>
                  <td>
                    <div className="row-actions">
                      <button
                        className="btn btn-sm"
                        disabled={busyRow === s.id}
                        onClick={() => void runCrawl(s)}
                        data-hint="Fetch this site's posts/pages and refresh its content index. Reads content only — no links are changed."
                      >
                        Crawl
                      </button>
                      <button
                        className="btn btn-sm"
                        disabled={busyRow === s.id}
                        onClick={() => void runPlan(s)}
                        data-hint="Run the NLP link plan for this site to generate internal-link suggestions. Suggestions only — nothing is applied."
                      >
                        Plan
                      </button>
                      <button
                        className="btn btn-sm"
                        onClick={() => navigate(`/site/${s.id}/dashboard`)}
                        data-hint="Open this site's dashboard — health score, orphans, opportunities and recent jobs."
                      >
                        Dashboard
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {showAdd && (
        <Modal
          title="Add Site"
          onClose={() => setShowAdd(false)}
          footer={
            <>
              <button className="btn" onClick={() => setShowAdd(false)}>
                Cancel
              </button>
              <button
                className="btn btn-primary"
                disabled={saving || !domain.trim()}
                form="add-site-form"
                type="submit"
                data-hint="Create the site record and generate its API key. The key is shown once on the next screen."
              >
                {saving ? 'Creating…' : 'Create site'}
              </button>
            </>
          }
        >
          <form id="add-site-form" onSubmit={(e) => void submitAdd(e)} style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
            {addError && <div className="error-box">{addError}</div>}
            <label className="field">
              Domain
              <input type="text" value={domain} onChange={(e) => setDomain(e.target.value)} placeholder="example.com" autoFocus />
            </label>
            <label className="field">
              WordPress URL (optional)
              <input type="text" value={wpUrl} onChange={(e) => setWpUrl(e.target.value)} placeholder="https://example.com" />
            </label>
            <label className="field">
              Crawl mode
              <select value={mode} onChange={(e) => setMode(e.target.value as SiteMode)}>
                <option value="wp_rest">wp_rest — WordPress REST API</option>
                <option value="sitemap">sitemap — XML sitemap crawl</option>
              </select>
            </label>
          </form>
        </Modal>
      )}

      {createdSite && (
        <Modal
          title={`Site created: ${createdSite.domain}`}
          onClose={() => setCreatedSite(null)}
          footer={
            <button className="btn btn-primary" onClick={() => setCreatedSite(null)}>
              Done
            </button>
          }
        >
          <p style={{ margin: 0 }} className="muted">
            API key for the SiloForge Connect plugin:
          </p>
          <div className="apikey-box">
            <code>{createdSite.api_key}</code>
            <button
              className="btn btn-sm"
              onClick={() => void copyKey(createdSite.api_key)}
              data-hint="Copy this API key to your clipboard. It's shown only once — paste it into the SiloForge Connect plugin now."
            >
              {copied ? 'Copied ✓' : 'Copy'}
            </button>
          </div>
          <div className="warn-note">This key is shown once here — store it now.</div>
        </Modal>
      )}
    </div>
  );
}
