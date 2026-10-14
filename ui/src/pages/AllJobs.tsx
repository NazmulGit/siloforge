import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { cancelJob, fetchJobsV2, isUnavailable, v2ErrorMessage } from '../api2';
import { JobLogDrawer } from '../components/JobLogDrawer';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { StatusChip } from '../components/StatusChip';
import { FeatureUnavailable } from '../components/V2Notice';
import { fmtDate, fmtDuration, fmtMb, fmtMs } from '../format';
import { useSites } from '../sites';
import { useToast } from '../toast';
import type { JobProgress } from '../types';
import type { JobRunV2 } from '../types2';

const STATUSES = ['', 'queued', 'running', 'done', 'failed', 'cancelled'] as const;
const KINDS = [
  '',
  'crawl',
  'plan',
  'inject',
  'undo',
  'keyword_link',
  'link_health',
  'audit',
  'embed',
  'gsc',
  'backup',
  'maintenance',
];
const LIMITS = [50, 100, 250, 500];

function readProgress(job: JobRunV2): JobProgress | null {
  const p = (job.stats as { progress?: unknown } | null)?.progress;
  if (!p || typeof p !== 'object') return null;
  const { phase, pct } = p as { phase?: unknown; pct?: unknown };
  if (typeof pct !== 'number') return null;
  return { phase: typeof phase === 'string' ? phase : '', pct };
}

function isActive(j: JobRunV2): boolean {
  return j.status === 'queued' || j.status === 'running';
}

/** Global job queue (all sites the user can see) with resources, progress, cancel and logs. */
export function AllJobsPage() {
  const [params, setParams] = useSearchParams();
  const toast = useToast();
  const { sites } = useSites();
  const status = params.get('status') ?? '';
  const kind = params.get('kind') ?? '';
  const siteId = params.get('site_id') ?? '';
  const limit = Number(params.get('limit') ?? 100) || 100;

  const [jobs, setJobs] = useState<JobRunV2[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [refreshTick, setRefreshTick] = useState(0);
  const [logJob, setLogJob] = useState<JobRunV2 | null>(null);
  const [cancelling, setCancelling] = useState<string | null>(null);

  const domains = useMemo(() => new Map(sites.map((s) => [s.id, s.domain])), [sites]);

  const setFilter = useCallback(
    (key: string, value: string) => {
      const next = new URLSearchParams(params);
      if (value) next.set(key, value);
      else next.delete(key);
      setParams(next, { replace: true });
    },
    [params, setParams],
  );

  useEffect(() => {
    let alive = true;
    let timer: number | undefined;
    let failures = 0;
    const tick = async () => {
      try {
        const list = await fetchJobsV2({ status, kind, site_id: siteId, limit });
        if (!alive) return;
        setJobs(list);
        setError(null);
        failures = 0;
        if (list.some(isActive)) timer = window.setTimeout(() => void tick(), 4000);
      } catch (e) {
        if (!alive) return;
        if (isUnavailable(e)) {
          setUnavailable(true);
          return;
        }
        setError(v2ErrorMessage(e, 'Failed to load jobs'));
        failures++;
        // Back off on persistent errors instead of hammering a down server.
        timer = window.setTimeout(() => void tick(), Math.min(60000, 5000 * 2 ** Math.min(failures, 4)));
      }
    };
    void tick();
    return () => {
      alive = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [status, kind, siteId, limit, refreshTick]);

  async function cancel(j: JobRunV2) {
    if (!window.confirm(`Cancel ${j.kind} job${j.site_domain ? ` on ${j.site_domain}` : ''}?`)) return;
    setCancelling(j.id);
    try {
      await cancelJob(j.id);
      toast.push('Cancel requested', 'info');
      setRefreshTick((t) => t + 1);
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Cancel failed'), 'error');
    } finally {
      setCancelling(null);
    }
  }

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>All jobs</h1>
          <div className="page-sub">Every queued, running and finished job across your sites, with resource use and logs.</div>
        </div>
      </div>

      {unavailable ? (
        <div className="card">
          <FeatureUnavailable feature="The global job list" />
        </div>
      ) : (
        <>
          <div className="table-toolbar" style={{ marginBottom: 12, flexWrap: 'wrap' }}>
            <select className="input" value={status} onChange={(e) => setFilter('status', e.target.value)} aria-label="Status filter">
              {STATUSES.map((s) => (
                <option key={s} value={s}>
                  {s ? s : 'Any status'}
                </option>
              ))}
            </select>
            <select className="input" value={kind} onChange={(e) => setFilter('kind', e.target.value)} aria-label="Kind filter">
              {KINDS.map((k) => (
                <option key={k} value={k}>
                  {k ? k : 'Any kind'}
                </option>
              ))}
            </select>
            <select className="input" value={siteId} onChange={(e) => setFilter('site_id', e.target.value)} aria-label="Site filter">
              <option value="">All sites</option>
              {sites.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.domain}
                </option>
              ))}
            </select>
            <select
              className="input"
              value={limit}
              onChange={(e) => setFilter('limit', e.target.value === '100' ? '' : e.target.value)}
              aria-label="Rows"
            >
              {LIMITS.map((n) => (
                <option key={n} value={n}>
                  {n} rows
                </option>
              ))}
            </select>
            <div className="spacer" />
            <span className="muted" style={{ fontSize: 12.5 }}>
              Auto-refreshes while jobs are active.
            </span>
            <button className="btn btn-sm" onClick={() => setRefreshTick((t) => t + 1)}>
              Refresh
            </button>
          </div>

          {error && (
            <div className="error-box" style={{ marginBottom: 14 }}>
              {error}
            </div>
          )}

          {jobs === null ? (
            <Skeleton rows={8} />
          ) : jobs.length === 0 ? (
            <EmptyState title="No jobs match" hint="Change the filters, or start a Crawl or Plan from a site's Dashboard." />
          ) : (
            <div className="tbl-wrap">
              <table className="tbl">
                <thead>
                  <tr>
                    <th>Kind</th>
                    <th>Site</th>
                    <th>Status</th>
                    <th>Progress</th>
                    <th>Created</th>
                    <th className="num">Duration</th>
                    <th className="num" title="Peak resident memory of the whole SiloForge process while this job ran (includes jobs running at the same time)">
                      Peak RAM
                    </th>
                    <th className="num" title="Process-wide average CPU % / CPU seconds while this job ran (includes jobs running at the same time)">
                      CPU
                    </th>
                    <th>Error</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {jobs.map((j) => {
                    const prog = readProgress(j);
                    const res = j.resources ?? {};
                    const domain = j.site_domain ?? (j.site_id ? domains.get(j.site_id) : null);
                    return (
                      <tr key={j.id}>
                        <td className="mono">{j.kind}</td>
                        <td>
                          {j.site_id ? (
                            <Link to={`/site/${j.site_id}/jobs`}>{domain ?? j.site_id.slice(0, 8)}</Link>
                          ) : (
                            <span className="faint">system</span>
                          )}
                        </td>
                        <td>
                          <StatusChip status={j.status} />
                        </td>
                        <td style={{ minWidth: 140 }}>
                          {prog && isActive(j) ? (
                            <div className="mini-progress" title={`${prog.phase} ${Math.round(prog.pct)}%`}>
                              <div className="mini-progress-bar">
                                <div className="mini-progress-fill" style={{ width: `${Math.max(0, Math.min(100, prog.pct))}%` }} />
                              </div>
                              <span className="mini-progress-label">
                                {prog.phase ? `${prog.phase} · ` : ''}
                                {Math.round(prog.pct)}%
                              </span>
                            </div>
                          ) : (
                            <span className="faint">—</span>
                          )}
                        </td>
                        <td className="muted">{fmtDate(j.created_at)}</td>
                        <td className="num muted">
                          {res.wall_ms !== undefined && !isActive(j) ? fmtMs(res.wall_ms) : fmtDuration(j.started_at, j.finished_at)}
                        </td>
                        <td className="num muted">{fmtMb(res.peak_rss_mb)}</td>
                        <td className="num muted">
                          {res.avg_cpu_pct !== undefined ? `${res.avg_cpu_pct.toFixed(0)}%` : '—'}
                          {res.cpu_secs !== undefined ? ` · ${res.cpu_secs.toFixed(1)}s` : ''}
                          {(res.concurrent_jobs_max ?? 1) > 1 && (
                            <span className="faint" title="Other jobs ran at the same time; RAM/CPU are shared figures">
                              {' '}
                              (shared ×{res.concurrent_jobs_max})
                            </span>
                          )}
                        </td>
                        <td
                          style={{ maxWidth: 220, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', color: j.error ? 'var(--red)' : undefined }}
                          title={j.error ?? ''}
                        >
                          {j.error ?? '—'}
                        </td>
                        <td>
                          <div className="row-actions">
                            <button className="btn btn-sm" onClick={() => setLogJob(j)} data-hint="Open this job's log output.">
                              Logs
                            </button>
                            {isActive(j) && (
                              <button
                                className="btn btn-sm btn-danger"
                                disabled={cancelling === j.id}
                                onClick={() => void cancel(j)}
                                data-hint="Ask the runner to stop this job. Work already written stays; injections can be undone from History."
                              >
                                Cancel
                              </button>
                            )}
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}

      {logJob && (
        <JobLogDrawer
          jobId={logJob.id}
          title={`Logs — ${logJob.kind}${logJob.site_domain ? ` · ${logJob.site_domain}` : ''}`}
          initialStatus={logJob.status}
          onClose={() => setLogJob(null)}
        />
      )}
    </div>
  );
}
