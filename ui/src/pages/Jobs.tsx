import { useEffect, useState } from 'react';
import { useParams, useSearchParams } from 'react-router-dom';
import { fetchJobs } from '../api';
import { JobLogDrawer } from '../components/JobLogDrawer';
import { MetricsTrend } from '../components/MetricsTrend';
import { SchedulesPanel } from '../components/SchedulesPanel';
import { StatusChip } from '../components/StatusChip';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { fmtDate, fmtDuration } from '../format';
import type { JobRun } from '../types';

// Runs and schedules are two views of the same question — "when does work happen
// on this site" — so they live on one page rather than adding a sidebar entry.
// The run log stays the default tab; the schedule editor and metric trends sit
// behind the second one.
type JobsTab = 'runs' | 'schedules';

const TABS: Array<{ id: JobsTab; label: string; hint: string }> = [
  {
    id: 'runs',
    label: 'Runs',
    hint: 'Every crawl, plan, inject, undo, link-health and audit run for this site, newest first.',
  },
  {
    id: 'schedules',
    label: 'Schedules & trends',
    hint: 'Recurring jobs for this site plus how its link metrics have moved between runs.',
  },
];

function statsSummary(stats: Record<string, unknown>): string {
  const parts: string[] = [];
  for (const [k, v] of Object.entries(stats)) {
    if (typeof v === 'number' || typeof v === 'string' || typeof v === 'boolean') {
      parts.push(`${k}: ${String(v)}`);
    }
  }
  return parts.join(' · ');
}

function RunsTab({ siteId }: { siteId: string }) {
  const [jobs, setJobs] = useState<JobRun[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refreshTick, setRefreshTick] = useState(0);
  const [logJob, setLogJob] = useState<JobRun | null>(null);

  useEffect(() => {
    let alive = true;
    let timer: number | undefined;

    const tick = async () => {
      try {
        const list = await fetchJobs(siteId);
        if (!alive) return;
        setJobs(list);
        setError(null);
        // Auto-refresh every 5s while anything is queued or running.
        if (list.some((j) => j.status === 'queued' || j.status === 'running')) {
          timer = window.setTimeout(() => void tick(), 5000);
        }
      } catch (e) {
        if (!alive) return;
        setError(e instanceof Error ? e.message : 'Failed to load jobs');
        timer = window.setTimeout(() => void tick(), 5000);
      }
    };

    void tick();
    return () => {
      alive = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [siteId, refreshTick]);

  return (
    <div>
      <div className="table-toolbar" style={{ marginBottom: 12 }}>
        <span className="muted">Auto-refreshes while any job is queued or running.</span>
        <div className="spacer" />
        <button
          className="btn btn-sm"
          onClick={() => setRefreshTick((t) => t + 1)}
          data-hint="Reload the job list now. It already auto-refreshes while any job is queued or running."
        >
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
        <EmptyState title="No jobs yet" hint="Trigger a Crawl or Plan from the Dashboard or Sites page." />
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Kind</th>
                <th>Status</th>
                <th>Created</th>
                <th>Started</th>
                <th>Duration</th>
                <th>Stats</th>
                <th>Error</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {jobs.map((j) => (
                <tr key={j.id}>
                  <td className="mono">{j.kind}</td>
                  <td>
                    <StatusChip status={j.status} />
                  </td>
                  <td className="muted">{fmtDate(j.created_at)}</td>
                  <td className="muted">{fmtDate(j.started_at)}</td>
                  <td className="muted">{fmtDuration(j.started_at, j.finished_at)}</td>
                  <td className="muted" style={{ maxWidth: 300, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={statsSummary(j.stats)}>
                    {statsSummary(j.stats) || '—'}
                  </td>
                  <td style={{ maxWidth: 240, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', color: j.error ? 'var(--red)' : undefined }} title={j.error ?? ''}>
                    {j.error ?? '—'}
                  </td>
                  <td>
                    <button className="btn btn-sm" onClick={() => setLogJob(j)} data-hint="Open this job's log output (SiloForge 2.0+).">
                      Logs
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {logJob && (
        <JobLogDrawer jobId={logJob.id} title={`Logs — ${logJob.kind}`} initialStatus={logJob.status} onClose={() => setLogJob(null)} />
      )}
    </div>
  );
}

export function JobsPage() {
  const { siteId } = useParams<{ siteId: string }>();
  const [searchParams] = useSearchParams();
  const requested = searchParams.get('tab');
  const [tab, setTab] = useState<JobsTab>(requested === 'schedules' ? 'schedules' : 'runs');

  if (!siteId) return <EmptyState title="No site selected" hint="Pick a site from the selector above." />;

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Jobs</h1>
          <div className="page-sub">Run history, recurring schedules and how the numbers are moving.</div>
        </div>
      </div>

      <div className="tabs" role="tablist" aria-label="Jobs views">
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            id={`jobs-tab-${t.id}`}
            aria-selected={tab === t.id}
            aria-controls={`jobs-panel-${t.id}`}
            className={`tab${tab === t.id ? ' active' : ''}`}
            data-hint={t.hint}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>

      <div id={`jobs-panel-${tab}`} role="tabpanel" aria-labelledby={`jobs-tab-${tab}`} tabIndex={-1}>
        {tab === 'runs' ? (
          <RunsTab siteId={siteId} />
        ) : (
          <>
            <SchedulesPanel siteId={siteId} />
            <MetricsTrend siteId={siteId} />
          </>
        )}
      </div>
    </div>
  );
}
