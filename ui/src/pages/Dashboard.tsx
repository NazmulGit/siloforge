import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import {
  aiRefineOpt,
  fetchDashboard,
  fetchLinkHealth,
  fetchMetricsReport,
  triggerCrawl,
  triggerPlan,
  type AiRefineChoice,
} from '../api';
import { PostLink, UrlMono } from '../components/PostLink';
import { StatCard } from '../components/StatCard';
import { StatusChip } from '../components/StatusChip';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { fmtDate, fmtDuration, scoreTone } from '../format';
import { useJobTracker } from '../hooks';
import { useToast } from '../toast';
import {
  EDGE_STATUSES,
  SILO_TOPOLOGIES,
  TOPOLOGY_HINTS,
  type CannibalizationWarning,
  type DashboardData,
  type EdgeStatus,
  type LinkHealthSummary,
  type SiloTopology,
  type SiteMetrics,
} from '../types';

const STATUS_COLORS: Record<EdgeStatus, string> = {
  suggested: '#64748b',
  approved: '#0d9488',
  applied: '#16a34a',
  rejected: '#e11d48',
  failed: '#e11d48',
  reverted: '#d97706',
};

const TONE_COLORS = { good: '#16a34a', ok: '#0d9488', warn: '#d97706', bad: '#e11d48' } as const;

function HealthDial({ score }: { score: number }) {
  // The API reports health on a 0..100 scale (v2 contract); clamp defensively.
  const pct = Math.max(0, Math.min(100, Number.isFinite(score) ? score : 0));
  const r = 56;
  const c = 2 * Math.PI * r;
  const color = TONE_COLORS[scoreTone(pct)];
  return (
    <div className="dial-wrap">
      <svg width="132" height="132" viewBox="0 0 132 132" style={{ transform: 'rotate(-90deg)' }} aria-hidden="true">
        <circle cx="66" cy="66" r={r} fill="none" stroke="#e6ebf3" strokeWidth="10" />
        <circle
          cx="66"
          cy="66"
          r={r}
          fill="none"
          stroke={color}
          strokeWidth="10"
          strokeLinecap="round"
          strokeDasharray={`${(pct / 100) * c} ${c}`}
        />
      </svg>
      <div className="dial-num" style={{ color }}>
        {Math.round(pct)}
        <small>HEALTH</small>
      </div>
    </div>
  );
}

function EdgesByStatusBar({ counts }: { counts: Partial<Record<EdgeStatus, number>> }) {
  const total = EDGE_STATUSES.reduce((sum, s) => sum + (counts[s] ?? 0), 0);
  if (total === 0) return <div className="muted">No edges yet — run a Plan.</div>;
  return (
    <div>
      <div className="stackbar">
        {EDGE_STATUSES.map((s) => {
          const n = counts[s] ?? 0;
          if (n === 0) return null;
          return <span key={s} style={{ width: `${(n / total) * 100}%`, background: STATUS_COLORS[s] }} title={`${s}: ${n}`} />;
        })}
      </div>
      <div className="stackbar-legend">
        {EDGE_STATUSES.map((s) => {
          const n = counts[s] ?? 0;
          if (n === 0) return null;
          return (
            <span key={s}>
              <span className="dot" style={{ background: STATUS_COLORS[s] }} />
              {s} <b>{n}</b>
            </span>
          );
        })}
      </div>
    </div>
  );
}

/** A competing page name — a real PostLink when a URL is known, else plain text. */
function pageNode(url: string | undefined, title: string | undefined): ReactNode {
  if (url) return <PostLink url={url} title={title || url} />;
  return title || '';
}

/** Cannibalization warning → keyword + a renderable detail (links when URLs exist). */
function warningParts(w: CannibalizationWarning): { kw: string; detail: ReactNode } {
  if (typeof w === 'string') return { kw: '', detail: w };
  const kw = w.keyword ?? w.anchor ?? '';
  if (w.message) return { kw, detail: w.message };
  if (w.target_a_url || w.target_b_url || w.target_a_title || w.target_b_title) {
    return {
      kw,
      detail: (
        <>
          competing pages: {pageNode(w.target_a_url, w.target_a_title)} vs{' '}
          {pageNode(w.target_b_url, w.target_b_title)}
        </>
      ),
    };
  }
  const posts = w.posts ?? w.urls ?? [];
  if (posts.length) {
    return {
      kw,
      detail: (
        <>
          competing pages:{' '}
          {posts.map((p, i) => {
            const url = typeof p === 'string' ? undefined : p.url;
            const title = typeof p === 'string' ? p : (p.title ?? p.url);
            return (
              <span key={i}>
                {i > 0 && ', '}
                {pageNode(url, title)}
              </span>
            );
          })}
        </>
      ),
    };
  }
  return { kw, detail: 'multiple pages compete for this term' };
}

/** `—` for a metric the latest snapshot / link-health crawl has not produced yet. */
function statValue(v: number | null | undefined): string | number {
  return v === null || v === undefined ? '—' : v;
}

function tone(v: number | null | undefined, warnOnly = false): 'good' | 'warn' | 'bad' | undefined {
  if (v === null || v === undefined) return undefined;
  if (v === 0) return 'good';
  return warnOnly ? 'warn' : 'bad';
}

export function DashboardPage() {
  const { siteId } = useParams<{ siteId: string }>();
  const navigate = useNavigate();
  const toast = useToast();
  const track = useJobTracker();

  const [data, setData] = useState<DashboardData | null>(null);
  // Technical-hygiene numbers come from the link-health crawl and the metric
  // snapshots, not from /dashboard — both are optional, so a site that has never
  // run either still renders (with em-dashes) instead of erroring.
  const [health, setHealth] = useState<LinkHealthSummary | null>(null);
  const [metrics, setMetrics] = useState<SiteMetrics | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [topology, setTopology] = useState<SiloTopology>('reverse');
  const [aiRefine, setAiRefine] = useState<AiRefineChoice>('');
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    if (!siteId) return;
    setError(null);
    const extras = Promise.allSettled([
      fetchLinkHealth(siteId, { limit: 1 }),
      fetchMetricsReport(siteId, 1),
    ]);
    try {
      setData(await fetchDashboard(siteId));
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load dashboard');
    } finally {
      setLoading(false);
    }
    const [lh, mr] = await extras;
    setHealth(lh.status === 'fulfilled' ? lh.value.summary : null);
    setMetrics(mr.status === 'fulfilled' ? (mr.value.latest?.metrics ?? null) : null);
  }, [siteId]);

  useEffect(() => {
    setLoading(true);
    setData(null);
    setHealth(null);
    setMetrics(null);
    void load();
  }, [load]);

  async function runCrawl() {
    if (!siteId || busy) return;
    setBusy(true);
    try {
      const { job_run_id } = await triggerCrawl(siteId);
      track(siteId, job_run_id, 'Crawl', () => void load());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Crawl failed to start', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function runPlan() {
    if (!siteId || busy) return;
    setBusy(true);
    try {
      const { job_run_id } = await triggerPlan(siteId, topology, aiRefineOpt(aiRefine));
      track(siteId, job_run_id, `Plan (${topology})`, () => void load());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Plan failed to start', 'error');
    } finally {
      setBusy(false);
    }
  }

  // Gap D — Quick-Fix Orphans / Deadends: generate suggestions if none exist yet,
  // then route the operator to the Inbound (orphans) / Outbound (dead-ends) approval queue.
  async function quickFix(kind: 'orphans' | 'deadends') {
    if (!siteId || busy) return;
    const targetTab = kind === 'orphans' ? 'inbound' : 'outbound';
    const dest = `/site/${siteId}/tools?tab=${targetTab}`;
    const haveSuggestions = (data?.edges_by_status.suggested ?? 0) > 0;
    if (haveSuggestions) {
      navigate(dest);
      return;
    }
    setBusy(true);
    try {
      const { job_run_id } = await triggerPlan(siteId, topology);
      track(siteId, job_run_id, `Quick Fix ${kind}`, () => {
        void load();
        navigate(dest);
      });
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Quick Fix failed to start', 'error');
    } finally {
      setBusy(false);
    }
  }

  if (!siteId) return <EmptyState title="No site selected" hint="Pick a site from the selector above." />;

  // G5: null = link health never ran (unknown, not 0); undefined = older server.
  const lhUnknown = data != null && data.link_health_checked_at === null;

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>{data?.site.domain ?? 'Dashboard'}</h1>
          <div className="page-sub">Link health and recent activity.</div>
        </div>
        <div className="spacer" />
        <button
          className="btn"
          disabled={busy}
          onClick={() => void runCrawl()}
          data-hint="Fetches all posts/pages from the site and refreshes the content index — no links are changed."
        >
          Run Crawl
        </button>
        <select
          value={topology}
          onChange={(e) => setTopology(e.target.value as SiloTopology)}
          aria-label="Plan topology"
          data-hint={`Choose the silo shape the plan builds. ${topology}: ${TOPOLOGY_HINTS[topology]}.`}
        >
          {SILO_TOPOLOGIES.map((t) => (
            <option key={t} value={t} title={TOPOLOGY_HINTS[t]}>
              {t} — {TOPOLOGY_HINTS[t]}
            </option>
          ))}
        </select>
        <select
          value={aiRefine}
          onChange={(e) => setAiRefine(e.target.value as AiRefineChoice)}
          aria-label="AI anchor refinement"
          data-hint="AI anchor refinement for this plan: site default (Linking Rules → Anchor text), on, or off. Replies are validated; failures keep the heuristic anchors."
        >
          <option value="">AI anchors: site default</option>
          <option value="on">AI anchors: on</option>
          <option value="off">AI anchors: off</option>
        </select>
        <button
          className="btn btn-primary"
          disabled={busy}
          onClick={() => void runPlan()}
          data-hint="Runs the full NLP link plan: embeds content, scores every candidate by topical relevancy, and generates link suggestions. Can take a few minutes on large sites."
        >
          Run Plan
        </button>
      </div>

      {error && <div className="error-box" style={{ marginBottom: 14 }}>{error}</div>}

      {loading || !data ? (
        <Skeleton rows={10} />
      ) : (
        <>
          <div className="dash-grid">
            <div
              className="card dial-card"
              data-hint="Overall internal-linking health (0–100), combining orphan rate, link depth, distribution and duplication. Higher is better."
            >
              <HealthDial score={data.health_score} />
              <div className="stat-label">Link Health Score</div>
            </div>
            <StatCard label="Posts" value={data.posts} hint="Total posts and pages currently in the content index for this site." />
            <StatCard
              label="Orphans"
              value={data.orphans}
              tone={data.orphans > 0 ? 'bad' : 'good'}
              sub="no inbound links"
              hint="Pages with no incoming internal links — search engines and readers can't easily reach them."
            />
            <StatCard
              label="Dead ends"
              value={data.dead_ends}
              tone={data.dead_ends > 0 ? 'warn' : 'good'}
              sub="no outbound links"
              hint="Pages with no outbound internal links — they don't pass link equity onward to other pages."
            />
            <StatCard
              label="Avg links / post"
              value={data.avg_links_per_post.toFixed(1)}
              tone="accent"
              sub={`${data.clusters} clusters`}
              hint="Average number of internal links per post across the site."
            />
          </div>

          {/* LinkBoss-parity metric cards (Gap E / F). */}
          <div className="dash-grid dash-grid-4">
            <button
              type="button"
              className="stat-card stat-card-link"
              onClick={() => navigate(`/site/${siteId}/tools?tab=inbound`)}
              title="Suggested links waiting for approval"
              data-hint="Suggested links waiting for your review. Click to open the Tools → Inbound approval queue."
            >
              <div className="stat-label">Opportunities</div>
              <div className="stat-value tone-accent">{data.opportunities}</div>
              <div className="stat-sub">suggested links to review</div>
            </button>
            <StatCard
              label="HTTP Links"
              value={lhUnknown ? '—' : data.http_links}
              tone={lhUnknown ? undefined : data.http_links > 0 ? 'bad' : 'good'}
              sub={
                lhUnknown
                  ? 'run Link Health to check'
                  : data.http_links > 0
                    ? 'links not using HTTPS'
                    : 'all links secure'
              }
              hint="Internal links still pointing at insecure http:// URLs instead of https://."
            />
            <StatCard
              label="Outbound Domains"
              value={lhUnknown ? '—' : data.outbound_domains}
              sub={lhUnknown ? 'run Link Health to check' : 'distinct external domains'}
              hint="Number of distinct external domains your content links out to."
            />
            <StatCard
              label="Link Duplication"
              value={data.link_duplication}
              tone={data.link_duplication > 0 ? 'warn' : 'good'}
              sub={data.link_duplication > 0 ? 'duplicate link edges' : 'no duplicates'}
              hint="Repeated links pointing to the same target from the same page — usually worth trimming."
            />
          </div>

          {/* Technical hygiene (checklist E) + editorial isolation (F). */}
          <div className="dash-grid dash-grid-4">
            <button
              type="button"
              className="stat-card stat-card-link"
              onClick={() => navigate(`/site/${siteId}/link-health`)}
              title="Open the Link Health report"
              data-hint="Internal and external links whose target returned an error or could not be reached. Click to open the Link Health report."
            >
              <div className="stat-label">Broken links</div>
              <div className={`stat-value${tone(health?.broken ?? metrics?.broken_links) ? ` tone-${tone(health?.broken ?? metrics?.broken_links)}` : ''}`}>
                {statValue(health?.broken ?? metrics?.broken_links)}
              </div>
              <div className="stat-sub">
                {health || metrics ? 'dead links found by the last check' : 'run a link health check'}
              </div>
            </button>
            <button
              type="button"
              className="stat-card stat-card-link"
              onClick={() => navigate(`/site/${siteId}/link-health`)}
              title="Open the Link Health report"
              data-hint="Internal links carrying rel=nofollow / sponsored / ugc. On your own site this is almost always unintentional and stops equity flowing."
            >
              <div className="stat-label">Nofollow internal</div>
              <div className={`stat-value${tone(metrics?.nofollow_links ?? health?.nofollow, true) ? ` tone-${tone(metrics?.nofollow_links ?? health?.nofollow, true)}` : ''}`}>
                {statValue(metrics?.nofollow_links ?? health?.nofollow)}
              </div>
              <div className="stat-sub">internal links blocking equity</div>
            </button>
            <button
              type="button"
              className="stat-card stat-card-link"
              onClick={() => navigate(`/site/${siteId}/link-health`)}
              title="Open the Link Health report"
              data-hint="Links that take more than two hops to reach their destination. Each hop leaks a little equity — point the link at the final URL instead."
            >
              <div className="stat-label">Redirect chains</div>
              <div className={`stat-value${tone(metrics?.redirect_chains_over_2 ?? health?.chains_over_two, true) ? ` tone-${tone(metrics?.redirect_chains_over_2 ?? health?.chains_over_two, true)}` : ''}`}>
                {statValue(metrics?.redirect_chains_over_2 ?? health?.chains_over_two)}
              </div>
              <div className="stat-sub">chains longer than 2 hops</div>
            </button>
            <StatCard
              label="Contextual orphans"
              value={statValue(metrics?.contextual_orphans)}
              tone={tone(metrics?.contextual_orphans)}
              sub={
                metrics?.nav_only_orphans !== undefined
                  ? `${metrics.nav_only_orphans} reachable only from nav`
                  : 'no in-body inbound link'
              }
              hint="Pages with no editorial in-body link pointing at them. They may still be crawlable through the menu or footer — that makes them reachable, not related. A page only the nav links to has no topical context telling search engines what it is about."
            />
          </div>

          <div className="card">
            <div className="card-title">Quick Fix</div>
            <div className="quickfix-row">
              <button
                className="btn"
                disabled={busy}
                onClick={() => void quickFix('orphans')}
                title="Generates & routes internal-link suggestions for orphan pages."
                data-hint="Generates inbound-link suggestions for orphan pages (running a Plan first if none exist), then drops you into the Inbound approval queue. No links are applied until you approve and inject."
              >
                Quick Fix Orphans <span className="count">{data.orphans}</span>
              </button>
              <button
                className="btn"
                disabled={busy}
                onClick={() => void quickFix('deadends')}
                title="Generates & routes internal-link suggestions for dead-end pages."
                data-hint="Generates outbound-link suggestions for dead-end pages (running a Plan first if none exist), then opens the Outbound approval queue. Nothing is written until you approve and inject."
              >
                Quick Fix Deadends <span className="count">{data.dead_ends}</span>
              </button>
              <span className="faint quickfix-hint">
                Generates suggestions if none exist, then routes you to the approval queue.
              </span>
            </div>
          </div>

          <div className="card">
            <div className="card-title">Edges by status</div>
            <EdgesByStatusBar counts={data.edges_by_status} />
          </div>

          <div className="dash-cols">
            <div className="card">
              <div className="card-title">Cannibalization warnings</div>
              {data.cannibalization_warnings.length === 0 ? (
                <div className="muted">No cannibalization detected.</div>
              ) : (
                <ul className="warn-list">
                  {data.cannibalization_warnings.map((w, i) => {
                    const { kw, detail } = warningParts(w);
                    return (
                      <li key={i}>
                        {kw && <span className="warn-kw">“{kw}”</span>}
                        <span>{detail}</span>
                      </li>
                    );
                  })}
                </ul>
              )}
            </div>

            <div className="card">
              <div className="card-title">Recent jobs</div>
              {data.recent_jobs.length === 0 ? (
                <div className="muted">No jobs yet.</div>
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
                    {data.recent_jobs.map((j) => (
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
          </div>

          {/* Gap G — orphan pages with a per-row Add-link shortcut into Tools → Inbound. */}
          <div className="card">
            <div className="card-title">Orphan pages</div>
            {data.orphan_posts.length === 0 ? (
              <div className="muted">No orphan pages — every post has at least one inbound link.</div>
            ) : (
              <table className="tbl">
                <thead>
                  <tr>
                    <th>Title</th>
                    <th>URL</th>
                    <th style={{ width: 110 }}>Action</th>
                  </tr>
                </thead>
                <tbody>
                  {data.orphan_posts.map((o) => (
                    <tr key={o.post_id}>
                      <td>
                        <PostLink url={o.url} title={o.title || '(untitled)'} />
                      </td>
                      <td style={{ maxWidth: 320 }}>
                        <UrlMono url={o.url} />
                      </td>
                      <td>
                        <button
                          className="btn btn-sm btn-primary"
                          onClick={() => navigate(`/site/${siteId}/tools?tab=inbound&target=${encodeURIComponent(o.post_id)}`)}
                          title="Build inbound links for this orphan page"
                          data-hint="Opens the Inbound tool focused on this orphan so you can add internal links pointing to it."
                        >
                          Add link
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
        </>
      )}
    </div>
  );
}
