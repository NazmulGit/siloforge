import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { downloadLogsCsv, fetchLogs, fetchLogStats, isUnavailable, v2ErrorMessage } from '../api2';
import { JobLogDrawer } from '../components/JobLogDrawer';
import { Skeleton } from '../components/Skeleton';
import { Toggle } from '../components/Toggle';
import { FeatureUnavailable } from '../components/V2Notice';
import { HelpButton } from '../docs';
import { fmtDate } from '../format';
import { useSites } from '../sites';
import { useToast } from '../toast';
import type { LogRow, LogsQuery, LogStats } from '../types2';

const SOURCES = ['ui', 'api', 'mcp', 'job', 'system', 'auth', 'notify'] as const;
const STATUSES = ['ok', 'error', 'denied'] as const;
const RANGES: Array<{ id: string; label: string; ms: number | null }> = [
  { id: '1h', label: 'Last hour', ms: 3_600_000 },
  { id: '24h', label: 'Last 24 hours', ms: 86_400_000 },
  { id: '7d', label: 'Last 7 days', ms: 7 * 86_400_000 },
  { id: '30d', label: 'Last 30 days', ms: 30 * 86_400_000 },
  { id: 'all', label: 'All time', ms: null },
];
const PAGE = 100;
const TAIL_MS = 3000;

function sinceFor(range: string): string | undefined {
  const r = RANGES.find((x) => x.id === range);
  return r?.ms ? new Date(Date.now() - r.ms).toISOString() : undefined;
}

/** Job rows carry the job id in `target` (or detail.job_id). */
function jobIdOf(row: LogRow): string | null {
  if (row.source !== 'job') return null;
  const d = row.detail as Record<string, unknown> | null | undefined;
  const fromDetail = d && typeof d === 'object' && typeof d.job_id === 'string' ? d.job_id : null;
  return fromDetail ?? row.target ?? null;
}

function detailText(detail: unknown): string {
  if (detail === null || detail === undefined || detail === '') return '(no detail)';
  if (typeof detail === 'string') {
    try {
      return JSON.stringify(JSON.parse(detail), null, 2);
    } catch {
      return detail;
    }
  }
  return JSON.stringify(detail, null, 2);
}

function Chips<T extends string>({
  label,
  options,
  value,
  onChange,
  counts,
}: {
  label: string;
  options: readonly T[];
  value: T | '';
  onChange: (v: T | '') => void;
  counts?: Record<string, number>;
}) {
  return (
    <div className="chip-filter" role="group" aria-label={label}>
      <button type="button" className={`fchip${value === '' ? ' on' : ''}`} aria-pressed={value === ''} onClick={() => onChange('')}>
        All
      </button>
      {options.map((o) => (
        <button key={o} type="button" className={`fchip fchip-${o}${value === o ? ' on' : ''}`} aria-pressed={value === o} onClick={() => onChange(value === o ? '' : o)}>
          {o}
          {counts && counts[o] !== undefined && <span className="fchip-n">{counts[o].toLocaleString()}</span>}
        </button>
      ))}
    </div>
  );
}

export function LogsPage() {
  const toast = useToast();
  const { sites } = useSites();
  const [source, setSource] = useState<(typeof SOURCES)[number] | ''>('');
  const [status, setStatus] = useState<(typeof STATUSES)[number] | ''>('');
  const [actor, setActor] = useState('');
  const [siteId, setSiteId] = useState('');
  const [range, setRange] = useState('24h');
  const [qInput, setQInput] = useState('');
  const [q, setQ] = useState('');
  const [tail, setTail] = useState(false);
  const [rows, setRows] = useState<LogRow[] | null>(null);
  const [nextBefore, setNextBefore] = useState<number | null>(null);
  const [stats, setStats] = useState<LogStats | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [open, setOpen] = useState<Set<number>>(new Set());
  const [job, setJob] = useState<{ id: string; title: string } | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const knownActors = useRef<Set<string>>(new Set());
  const rowsRef = useRef<LogRow[] | null>(null);
  rowsRef.current = rows;

  // Debounce the search box.
  useEffect(() => {
    const t = window.setTimeout(() => setQ(qInput.trim()), 350);
    return () => window.clearTimeout(t);
  }, [qInput]);

  const query = useMemo<LogsQuery>(
    () => ({ source: source || undefined, status: status || undefined, actor: actor || undefined, site_id: siteId || undefined, q: q || undefined, since: sinceFor(range) }),
    // range is re-evaluated on each filter change; tail keeps "since" fresh enough.
    [source, status, actor, siteId, q, range],
  );

  const load = useCallback(async () => {
    setRows(null);
    setError(null);
    setOpen(new Set());
    try {
      const [page, st] = await Promise.all([fetchLogs({ ...query, limit: PAGE }), fetchLogStats(query.since).catch(() => null)]);
      page.logs.forEach((r) => r.actor && knownActors.current.add(r.actor));
      setRows(page.logs);
      setNextBefore(page.next_before_id);
      setStats(st);
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(v2ErrorMessage(e, 'Failed to load logs'));
      setRows([]);
    }
  }, [query]);

  useEffect(() => {
    void load();
  }, [load]);

  // Live tail: poll for rows newer than the newest one shown.
  useEffect(() => {
    if (!tail || unavailable) return;
    let alive = true;
    let busy = false;
    const timer = window.setInterval(() => {
      if (busy) return;
      busy = true;
      const maxId = (rowsRef.current ?? []).reduce((m, r) => Math.max(m, r.id), 0);
      void fetchLogs({ ...query, limit: PAGE, after_id: maxId || undefined })
        .then((page) => {
          if (!alive) return;
          const fresh = page.logs.filter((r) => r.id > maxId);
          if (fresh.length === 0) return;
          fresh.forEach((r) => r.actor && knownActors.current.add(r.actor));
          setRows((now) => {
            const ids = new Set((now ?? []).map((r) => r.id));
            return [...fresh.filter((r) => !ids.has(r.id)).sort((a, b) => b.id - a.id), ...(now ?? [])].slice(0, 2000);
          });
        })
        .catch(() => {
          /* transient — keep tailing */
        })
        .finally(() => {
          busy = false;
        });
    }, TAIL_MS);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [tail, query, unavailable]);

  async function loadMore() {
    if (nextBefore === null) return;
    setLoadingMore(true);
    try {
      const page = await fetchLogs({ ...query, limit: PAGE, before_id: nextBefore });
      setRows((cur) => {
        const ids = new Set((cur ?? []).map((r) => r.id));
        return [...(cur ?? []), ...page.logs.filter((r) => !ids.has(r.id))];
      });
      setNextBefore(page.next_before_id);
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Failed to load older entries'), 'error');
    } finally {
      setLoadingMore(false);
    }
  }

  async function exportCsv() {
    try {
      await downloadLogsCsv(query);
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Export failed'), 'error');
    }
  }

  const siteName = useMemo(() => new Map(sites.map((s) => [s.id, s.domain])), [sites]);
  const actors = [...knownActors.current].sort();
  if (actor && !actors.includes(actor)) actors.unshift(actor);

  if (unavailable) {
    return (
      <div>
        <div className="page-head">
          <h1>Logs</h1>
        </div>
        <div className="card">
          <FeatureUnavailable feature="The unified log" />
        </div>
      </div>
    );
  }

  const total = stats?.total ?? Object.values(stats?.by_source ?? {}).reduce((a, b) => a + b, 0);

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Logs</h1>
          <div className="page-sub">Everything that happened: UI and API calls, MCP tool calls, jobs, sign-ins, alerts and server events.</div>
        </div>
        <div className="spacer" />
        <HelpButton />
        <button className="btn" onClick={() => void exportCsv()}>
          Export CSV
        </button>
      </div>

      {stats && (
        <div className="log-stats">
          <div className="log-stat">
            <span className="log-stat-n">{total.toLocaleString()}</span>
            <span className="log-stat-l">events</span>
          </div>
          <div className="log-stat tone-bad">
            <span className="log-stat-n">{(stats.by_status.error ?? 0).toLocaleString()}</span>
            <span className="log-stat-l">errors</span>
          </div>
          <div className="log-stat tone-warn">
            <span className="log-stat-n">{(stats.by_status.denied ?? 0).toLocaleString()}</span>
            <span className="log-stat-l">denied</span>
          </div>
          <div className="log-stat">
            <span className="log-stat-n">{(stats.by_source.mcp ?? 0).toLocaleString()}</span>
            <span className="log-stat-l">MCP calls</span>
          </div>
        </div>
      )}

      <div className="card log-filters">
        <Chips label="Source" options={SOURCES} value={source} onChange={setSource} counts={stats?.by_source} />
        <Chips label="Status" options={STATUSES} value={status} onChange={setStatus} counts={stats?.by_status} />
        <div className="log-filter-row">
          <input className="input" type="search" placeholder="Search action, target, actor" aria-label="Search logs" value={qInput} onChange={(e) => setQInput(e.target.value)} />
          <select aria-label="Actor" value={actor} onChange={(e) => setActor(e.target.value)}>
            <option value="">Any actor</option>
            {actors.map((a) => (
              <option key={a} value={a}>
                {a}
              </option>
            ))}
          </select>
          <select aria-label="Site" value={siteId} onChange={(e) => setSiteId(e.target.value)}>
            <option value="">Any site</option>
            {sites.map((s) => (
              <option key={s.id} value={s.id}>
                {s.domain}
              </option>
            ))}
          </select>
          <select aria-label="Time range" value={range} onChange={(e) => setRange(e.target.value)}>
            {RANGES.map((r) => (
              <option key={r.id} value={r.id}>
                {r.label}
              </option>
            ))}
          </select>
          <label className="inline-toggle">
            <Toggle checked={tail} onChange={setTail} label="Live tail" />
            Live tail{tail && <span className="live-dot" aria-hidden="true" />}
          </label>
        </div>
      </div>

      <div className="card">
        {error && <div className="error-box">{error}</div>}
        {rows === null ? (
          <Skeleton rows={8} />
        ) : rows.length === 0 ? (
          <div className="empty">
            <div className="empty-title">No events match these filters</div>
            <div className="empty-hint">Widen the time range or clear a filter.</div>
          </div>
        ) : (
          <div className="tbl-wrap">
            <table className="tbl log-tbl">
              <thead>
                <tr>
                  <th>Time</th>
                  <th>Source</th>
                  <th>Status</th>
                  <th>Action</th>
                  <th>Actor</th>
                  <th>Site</th>
                  <th className="num">ms</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => {
                  const isOpen = open.has(r.id);
                  const jobId = jobIdOf(r);
                  return (
                    <Fragment key={r.id}>
                      <tr
                        className={`log-row${isOpen ? ' open' : ''}`}
                        tabIndex={0}
                        aria-expanded={isOpen}
                        onClick={() =>
                          setOpen((cur) => {
                            const n = new Set(cur);
                            if (n.has(r.id)) n.delete(r.id);
                            else n.add(r.id);
                            return n;
                          })
                        }
                        onKeyDown={(e) => {
                          if (e.key === 'Enter' || e.key === ' ') {
                            e.preventDefault();
                            (e.currentTarget as HTMLElement).click();
                          }
                        }}
                      >
                        <td className="muted nowrap">{fmtDate(r.ts)}</td>
                        <td>
                          <span className={`src-tag src-${r.source}`}>{r.source}</span>
                        </td>
                        <td>
                          <span className={`st-dot st-${r.status}`}>{r.status}</span>
                          {r.http_status ? <span className="faint"> {r.http_status}</span> : null}
                        </td>
                        <td className="mono-sm log-action">{r.action}</td>
                        <td className="mono-sm">{r.actor ?? '—'}</td>
                        <td className="muted">{r.site_id ? siteName.get(r.site_id) ?? r.site_id.slice(0, 8) : '—'}</td>
                        <td className="num muted">{r.duration_ms ?? ''}</td>
                      </tr>
                      {isOpen && (
                        <tr className="log-detail">
                          <td colSpan={7}>
                            <div className="log-detail-meta">
                              <span>id {r.id}</span>
                              {r.target && <span>target {r.target}</span>}
                              {jobId && (
                                <button className="btn btn-sm" onClick={() => setJob({ id: jobId, title: r.action })}>
                                  Open job log
                                </button>
                              )}
                            </div>
                            <pre className="code-pre">{detailText(r.detail)}</pre>
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        {rows && rows.length > 0 && nextBefore !== null && (
          <div className="form-actions" style={{ justifyContent: 'center', marginTop: 12 }}>
            <button className="btn" disabled={loadingMore} onClick={() => void loadMore()}>
              {loadingMore ? 'Loading…' : 'Load older'}
            </button>
          </div>
        )}
      </div>
      {job && <JobLogDrawer jobId={job.id} title={job.title} onClose={() => setJob(null)} />}
    </div>
  );
}
