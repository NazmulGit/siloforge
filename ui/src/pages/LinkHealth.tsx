// Link Health report (checklist E — technical hygiene).
//
// Four findings, one crawl: broken links, redirect chains, internal links that
// silently carry rel=nofollow, and breadcrumb coverage. Everything here is
// read-only — the only write is enqueuing the crawl itself.
import { useCallback, useEffect, useMemo, useState } from 'react';
import { useParams } from 'react-router-dom';
import { ApiError, downloadLinkHealthCsv, fetchLinkHealth, runLinkHealth } from '../api';
import { PostLink } from '../components/PostLink';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { StatCard } from '../components/StatCard';
import { fmtDate } from '../format';
import { useJobTracker } from '../hooks';
import { useToast } from '../toast';
import { redirectHops, type LinkHealthReport, type LinkHealthRow, type RedirectHop } from '../types';

type HealthTab = 'broken' | 'redirects' | 'nofollow' | 'breadcrumbs';

const TABS: Array<{ id: HealthTab; label: string; hint: string }> = [
  {
    id: 'broken',
    label: 'Broken',
    hint: 'Links whose target returned an error (4xx / 5xx) or could not be fetched at all — dead ends for readers and crawlers.',
  },
  {
    id: 'redirects',
    label: 'Redirect chains',
    hint: 'Links that do not land on their target directly. Every hop leaks a little equity and adds latency — link to the final URL instead.',
  },
  {
    id: 'nofollow',
    label: 'Nofollow',
    hint: 'Internal links carrying rel=nofollow / sponsored / ugc. On your own site this is almost always unintentional and blocks equity flow.',
  },
  {
    id: 'breadcrumbs',
    label: 'Breadcrumbs',
    hint: 'Per-post breadcrumb coverage: visible trail, BreadcrumbList JSON-LD, and any mismatch between the two.',
  },
];

const ROW_LIMIT = 250;

/** A `href` cell: a real link when the URL is absolute, plain mono text otherwise. */
function HrefCell({ row }: { row: LinkHealthRow }) {
  const target = row.resolved_url ?? row.requested_url;
  const absolute = /^https?:\/\//i.test(target);
  if (!absolute) {
    return (
      <span className="mono-sm" title={row.href}>
        {row.href}
      </span>
    );
  }
  return (
    <a
      href={target}
      target="_blank"
      rel="noopener noreferrer"
      className="urlmono"
      title={target}
      data-hint="Open the link target in a new tab (right-click to copy the address)"
    >
      {row.href}
    </a>
  );
}

/** Source post cell — a PostLink when the crawl knew which post the link came from. */
function SourceCell({ row }: { row: LinkHealthRow }) {
  if (!row.source_url) return <span className="faint">—</span>;
  return <PostLink url={row.source_url} title={row.source_title || row.source_url} />;
}

function StatusCell({ code }: { code: number | null }) {
  if (code === null) return <span className="chip chip-failed">no response</span>;
  const tone = code >= 500 ? 'failed' : code >= 400 ? 'rejected' : code >= 300 ? 'reverted' : 'done';
  return <span className={`chip chip-${tone}`}>{code}</span>;
}

/** `/a → 301 → /b → 302 → /c` — the whole hop chain, rendered inline. */
function HopChain({ hops, requested }: { hops: RedirectHop[]; requested: string }) {
  if (hops.length === 0) {
    return (
      <span className="mono-sm" title={requested}>
        {requested}
      </span>
    );
  }
  return (
    <span className="hopchain">
      {hops.map((h, i) => (
        <span key={`${h.order}-${h.url}`} className="hopchain-seg">
          {i > 0 && (
            <span className={`hopchain-status${h.status === 302 || h.status === 307 ? ' temp' : ''}`}>
              {h.status || '?'}
            </span>
          )}
          <span className="hopchain-url" title={h.url}>
            {h.url}
          </span>
        </span>
      ))}
    </span>
  );
}

function RelChips({ row }: { row: LinkHealthRow }) {
  const flags: string[] = [];
  if (row.is_nofollow) flags.push('nofollow');
  if (row.is_sponsored) flags.push('sponsored');
  if (row.is_ugc) flags.push('ugc');
  if (flags.length === 0) return <span className="faint">{row.rel || '—'}</span>;
  return (
    <span className="chip-list">
      {flags.map((f) => (
        <span key={f} className="chip chip-rejected">
          {f}
        </span>
      ))}
    </span>
  );
}

function ScopeChip({ scope }: { scope: string }) {
  return <span className={`chip chip-${scope === 'internal' ? 'approved' : 'mode'}`}>{scope}</span>;
}

function download(name: string, blob: Blob): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

// ---------------------------------------------------------------------------
// Tab bodies
// ---------------------------------------------------------------------------

function BrokenTab({ rows }: { rows: LinkHealthRow[] }) {
  if (rows.length === 0) {
    return (
      <EmptyState
        title="No broken links"
        hint="Every link the last crawl followed resolved to a live page."
      />
    );
  }
  return (
    <div className="tbl-wrap">
      <table className="tbl">
        <thead>
          <tr>
            <th>Source post</th>
            <th>Href</th>
            <th>Anchor</th>
            <th style={{ width: 110 }}>Status</th>
            <th style={{ width: 100 }}>Placement</th>
            <th>Error</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.id}>
              <td style={{ maxWidth: 260 }}>
                <SourceCell row={r} />
              </td>
              <td style={{ maxWidth: 300 }}>
                <HrefCell row={r} />
              </td>
              <td className="anchor-cell" title={r.anchor}>
                {r.anchor ? `“${r.anchor}”` : <span className="faint">—</span>}
              </td>
              <td>
                <StatusCell code={r.status_code} />
              </td>
              <td className="muted">{r.placement}</td>
              <td style={{ maxWidth: 240, color: r.error ? 'var(--red)' : undefined }} title={r.error ?? ''}>
                {r.error ?? '—'}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function RedirectsTab({ rows }: { rows: LinkHealthRow[] }) {
  if (rows.length === 0) {
    return (
      <EmptyState
        title="No redirected links"
        hint="Every link points straight at its final URL — nothing to clean up."
      />
    );
  }
  return (
    <div className="tbl-wrap">
      <table className="tbl">
        <thead>
          <tr>
            <th>Source post</th>
            <th>Hop chain</th>
            <th className="num" style={{ width: 70 }}>
              Hops
            </th>
            <th style={{ width: 220 }}>Warnings</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => {
            const hops = redirectHops(r.redirect_chain);
            const longChain = r.redirect_hops > 2;
            return (
              <tr key={r.id}>
                <td style={{ maxWidth: 240 }}>
                  <SourceCell row={r} />
                </td>
                <td style={{ maxWidth: 520 }}>
                  <HopChain hops={hops} requested={r.requested_url} />
                </td>
                <td className={`num${longChain ? ' val-bad' : ''}`}>{r.redirect_hops}</td>
                <td>
                  <span className="chip-list">
                    {longChain && (
                      <span
                        className="chip chip-rejected"
                        title="More than two hops — link directly to the final URL"
                      >
                        ⚠ {r.redirect_hops} hops
                      </span>
                    )}
                    {r.has_temp_redirect && (
                      <span
                        className="chip chip-reverted"
                        title="302 / 307 is a temporary redirect — search engines may keep the old URL indexed"
                      >
                        ⚠ temporary
                      </span>
                    )}
                    {r.has_redirect_loop && (
                      <span className="chip chip-failed" title="The chain returns to a URL it already visited">
                        ⚠ loop
                      </span>
                    )}
                    {!longChain && !r.has_temp_redirect && !r.has_redirect_loop && (
                      <span className="faint">single permanent hop</span>
                    )}
                  </span>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function NofollowTab({ rows }: { rows: LinkHealthRow[] }) {
  const internal = rows.filter((r) => r.scope === 'internal');
  if (rows.length === 0) {
    return (
      <EmptyState
        title="No nofollow links found"
        hint="No internal link is carrying rel=nofollow, sponsored or ugc."
      />
    );
  }
  return (
    <>
      {internal.length > 0 && (
        <div className="info-note" style={{ marginBottom: 14 }}>
          <span className="info-note-icon" aria-hidden="true">
            ⚠
          </span>
          <span className="info-note-text">
            <b>{internal.length}</b> internal link{internal.length === 1 ? '' : 's'} carry a{' '}
            <code className="mono">rel</code> that blocks equity. On your own site this is almost always
            unintentional — remove the attribute unless you meant it.
          </span>
        </div>
      )}
      <div className="tbl-wrap">
        <table className="tbl">
          <thead>
            <tr>
              <th>Source post</th>
              <th>Href</th>
              <th>Anchor</th>
              <th style={{ width: 100 }}>Scope</th>
              <th style={{ width: 190 }}>rel</th>
              <th style={{ width: 100 }}>Placement</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.id}>
                <td style={{ maxWidth: 240 }}>
                  <SourceCell row={r} />
                </td>
                <td style={{ maxWidth: 300 }}>
                  <HrefCell row={r} />
                </td>
                <td className="anchor-cell" title={r.anchor}>
                  {r.anchor ? `“${r.anchor}”` : <span className="faint">—</span>}
                </td>
                <td>
                  <ScopeChip scope={r.scope} />
                </td>
                <td>
                  <RelChips row={r} />
                </td>
                <td className="muted">{r.placement}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}

/** Short note from `posts.breadcrumb_report` (camelCase report written by the link-health job). */
function crumbNote(raw: unknown): string {
  if (!raw || typeof raw !== 'object') return '';
  const r = raw as { issues?: unknown; count?: unknown; crumbs?: unknown };
  const issues = Array.isArray(r.issues) ? r.issues.filter((i): i is string => typeof i === 'string') : [];
  const count = typeof r.count === 'number' ? r.count : Array.isArray(r.crumbs) ? r.crumbs.length : 0;
  const parts: string[] = [];
  if (count > 0) parts.push(`${count} crumb${count === 1 ? '' : 's'}`);
  if (issues.length > 0) parts.push(issues[0] + (issues.length > 1 ? ` (+${issues.length - 1} more)` : ''));
  return parts.join(' · ');
}

function BreadcrumbsTab({ report }: { report: LinkHealthReport }) {
  const b = report.breadcrumbs;
  return (
    <>
      <div className="dash-grid dash-grid-4">
        <StatCard
          label="Posts checked"
          value={b.checked}
          sub={`of ${b.posts} posts`}
          hint="Posts the link-health crawl has inspected for breadcrumbs."
        />
        <StatCard
          label="Visible trail"
          value={b.with_visible}
          tone={b.checked > 0 && b.with_visible < b.checked ? 'warn' : 'good'}
          sub="breadcrumb markup on page"
          hint="Posts rendering a visible breadcrumb trail readers can click."
        />
        <StatCard
          label="JSON-LD"
          value={b.with_schema}
          tone={b.checked > 0 && b.with_schema < b.checked ? 'warn' : 'good'}
          sub="BreadcrumbList schema"
          hint="Posts emitting BreadcrumbList structured data for search engines."
        />
        <StatCard
          label="Mismatched"
          value={b.mismatched}
          tone={b.mismatched > 0 ? 'bad' : 'good'}
          sub={b.mismatched > 0 ? 'visible ≠ JSON-LD' : 'in agreement'}
          hint="Posts where the visible trail and the structured data disagree — one of the two is missing."
        />
      </div>

      {b.checked === 0 ? (
        <EmptyState
          title="No breadcrumb data yet"
          hint="Run a link health check — breadcrumbs are inspected as part of the same crawl."
        />
      ) : b.missing.length === 0 ? (
        <EmptyState title="Breadcrumbs complete" hint="Every checked post has both a visible trail and JSON-LD." />
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Post</th>
                <th style={{ width: 150 }}>Visible trail</th>
                <th style={{ width: 150 }}>JSON-LD</th>
                <th style={{ width: 150 }}>Verdict</th>
                <th>Details</th>
              </tr>
            </thead>
            <tbody>
              {b.missing.map((p) => {
                const mismatch = p.has_breadcrumbs !== p.has_breadcrumb_schema;
                return (
                  <tr key={p.post_id}>
                    <td style={{ maxWidth: 420 }}>
                      <PostLink url={p.url} title={p.title || p.url} />
                    </td>
                    <td>
                      <span className={`chip chip-${p.has_breadcrumbs ? 'done' : 'rejected'}`}>
                        {p.has_breadcrumbs ? 'present' : 'missing'}
                      </span>
                    </td>
                    <td>
                      <span className={`chip chip-${p.has_breadcrumb_schema ? 'done' : 'rejected'}`}>
                        {p.has_breadcrumb_schema ? 'present' : 'missing'}
                      </span>
                    </td>
                    <td>
                      {mismatch ? (
                        <span className="chip chip-reverted" title="Only one of the two is present">
                          mismatch
                        </span>
                      ) : (
                        <span className="chip chip-failed">both missing</span>
                      )}
                    </td>
                    <td className="muted" style={{ maxWidth: 360 }}>
                      {crumbNote(p.breadcrumb_report) || '—'}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function LinkHealthPage() {
  const { siteId } = useParams<{ siteId: string }>();
  const toast = useToast();
  const track = useJobTracker();

  const [tab, setTab] = useState<HealthTab>('broken');
  const [report, setReport] = useState<LinkHealthReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    if (!siteId) return;
    setError(null);
    try {
      setReport(await fetchLinkHealth(siteId, { limit: ROW_LIMIT }));
    } catch (e) {
      const msg =
        e instanceof ApiError && e.status === 404
          ? 'No link-health data for this site yet — run a check to build the report.'
          : e instanceof Error
            ? e.message
            : 'Failed to load link health';
      setError(msg);
      setReport(null);
    } finally {
      setLoading(false);
    }
  }, [siteId]);

  useEffect(() => {
    setLoading(true);
    setReport(null);
    void load();
  }, [load]);

  async function runCheck() {
    if (!siteId || busy) return;
    setBusy(true);
    try {
      const { job_run_id } = await runLinkHealth(siteId);
      track(siteId, job_run_id, 'Link health check', () => void load());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Link health check failed to start', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function exportCsv() {
    if (!siteId || busy) return;
    setBusy(true);
    try {
      download(`siloforge-link-health-${siteId}.csv`, await downloadLinkHealthCsv(siteId));
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Export failed', 'error');
    } finally {
      setBusy(false);
    }
  }

  const counts = useMemo(
    () => ({
      broken: report?.broken.length ?? 0,
      redirects: report?.redirects.length ?? 0,
      nofollow: report?.nofollow.length ?? 0,
      breadcrumbs: report?.breadcrumbs.missing.length ?? 0,
    }),
    [report],
  );

  if (!siteId) return <EmptyState title="No site selected" hint="Pick a site from the selector above." />;

  const s = report?.summary;

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Link Health</h1>
          <div className="page-sub">
            Broken links, redirect chains, unintentional nofollow and breadcrumb coverage.
            {s?.last_checked_at && <> Last checked {fmtDate(s.last_checked_at)}.</>}
          </div>
        </div>
        <div className="spacer" />
        <button
          className="btn"
          disabled={busy || !report}
          onClick={() => void exportCsv()}
          data-hint="Download every checked link — status, redirect chain, rel and placement — as a CSV file."
        >
          Export CSV
        </button>
        <button
          className="btn btn-primary"
          disabled={busy}
          onClick={() => void runCheck()}
          data-hint="Crawls every link in your published content and records its status, redirect chain and rel attributes. Reads only — no content is changed."
        >
          Run link health check
        </button>
      </div>

      {error && (
        <div className="error-box" style={{ marginBottom: 14 }}>
          {error}
        </div>
      )}

      {loading ? (
        <Skeleton rows={10} />
      ) : !report ? (
        <EmptyState
          title="No link health report yet"
          hint="Run a link health check to crawl every internal and external link on the site."
        />
      ) : (
        <>
          <div className="dash-grid dash-grid-4">
            <StatCard
              label="Broken links"
              value={s?.broken ?? 0}
              tone={(s?.broken ?? 0) > 0 ? 'bad' : 'good'}
              sub={`of ${s?.total_links ?? 0} checked`}
              hint="Links whose target returned an error or could not be reached at all."
            />
            <StatCard
              label="Redirect chains"
              value={s?.chains_over_two ?? 0}
              tone={(s?.chains_over_two ?? 0) > 0 ? 'warn' : 'good'}
              sub={`${s?.redirected ?? 0} redirected · ${s?.loops ?? 0} loops`}
              hint="Links taking more than two hops to reach their destination. Each hop leaks equity — link to the final URL."
            />
            <StatCard
              label="Nofollow links"
              value={s?.nofollow ?? 0}
              tone={(s?.nofollow ?? 0) > 0 ? 'warn' : 'good'}
              sub={`${s?.sponsored ?? 0} sponsored · ${s?.ugc ?? 0} ugc`}
              hint="Links carrying rel=nofollow / sponsored / ugc. Internal ones block equity flow and are usually a mistake."
            />
            <StatCard
              label="Internal links"
              value={s?.internal ?? 0}
              tone="accent"
              sub={`${s?.external ?? 0} external`}
              hint="How many of the checked links stay on your own domain."
            />
          </div>

          <div className="tabs" role="tablist" aria-label="Link health findings">
            {TABS.map((t) => (
              <button
                key={t.id}
                type="button"
                role="tab"
                id={`lh-tab-${t.id}`}
                aria-selected={tab === t.id}
                aria-controls={`lh-panel-${t.id}`}
                className={`tab${tab === t.id ? ' active' : ''}`}
                data-hint={t.hint}
                onClick={() => setTab(t.id)}
              >
                {t.label}
                <span className="count">{counts[t.id]}</span>
              </button>
            ))}
          </div>

          <div id={`lh-panel-${tab}`} role="tabpanel" aria-labelledby={`lh-tab-${tab}`} tabIndex={-1}>
            {tab === 'broken' && <BrokenTab rows={report.broken} />}
            {tab === 'redirects' && <RedirectsTab rows={report.redirects} />}
            {tab === 'nofollow' && <NofollowTab rows={report.nofollow} />}
            {tab === 'breadcrumbs' && <BreadcrumbsTab report={report} />}
          </div>

          {tab !== 'breadcrumbs' && counts[tab] >= ROW_LIMIT && (
            <div className="muted" style={{ marginTop: 10, fontSize: 12.5 }}>
              Showing the first {ROW_LIMIT} rows — export the CSV for the full list.
            </div>
          )}
        </>
      )}
    </div>
  );
}
