import { useCallback, useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { fetchErrors, fetchSystem, fetchSystemMetrics, isUnavailable, resolveError, v2ErrorMessage } from '../api2';
import { AreaChart, type ChartPoint } from '../components/AreaChart';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { StatCard } from '../components/StatCard';
import { FeatureUnavailable } from '../components/V2Notice';
import { fmtBytes, fmtDate, fmtMb, fmtUptime } from '../format';
import { useToast } from '../toast';
import { useUser } from '../user';
import type { ErrorRow, MetricSample, MetricsWindow, SystemInfo } from '../types2';

const WINDOWS: Array<{ id: MetricsWindow; label: string }> = [
  { id: '1h', label: 'Last hour' },
  { id: '24h', label: '24 hours' },
  { id: '7d', label: '7 days' },
];

const pct = (v: number) => `${v.toFixed(0)}%`;
const mb = (v: number) => fmtMb(v);

function series(samples: MetricSample[], pick: (s: MetricSample) => number): ChartPoint[] {
  const out: ChartPoint[] = [];
  for (const s of samples) {
    const t = new Date(s.ts).getTime();
    const v = pick(s);
    if (Number.isFinite(t) && Number.isFinite(v)) out.push({ t, v });
  }
  return out;
}

function jobCounts(info: SystemInfo): { running: number; queued: number; failed_24h?: number } {
  return info.jobs ?? { running: info.current?.jobs_running ?? 0, queued: info.current?.jobs_queued ?? 0 };
}

function SystemCards({ info }: { info: SystemInfo }) {
  const cur = info.current;
  const cap = info.capacity ?? {};
  const memTotal = cur?.mem_total_mb ?? info.mem_total_mb ?? cap.mem_total_mb ?? null;
  const logical = cap.logical_cpus ?? info.cpu_threads ?? null;
  const pool = cap.cpu_threads ?? info.pool_threads ?? null;
  const jobs = jobCounts(info);
  const host = info.hostname ?? cap.hostname ?? info.os ?? cap.os ?? undefined;
  return (
    <div className="usage-grid" style={{ marginBottom: 14 }}>
      <StatCard label="Version" value={info.version || '—'} sub={host} hint="SiloForge server version." />
      <StatCard
        label="Uptime"
        value={fmtUptime(info.uptime_s ?? info.uptime_secs)}
        sub={info.started_at ? `since ${fmtDate(info.started_at)}` : undefined}
        hint="How long the server process has been running."
      />
      <StatCard
        label="CPU now"
        value={cur ? pct(cur.cpu_pct) : '—'}
        sub={cur ? `SiloForge ${pct(cur.proc_cpu_pct)}` : undefined}
        tone={cur && cur.cpu_pct > 85 ? 'bad' : cur && cur.cpu_pct > 60 ? 'warn' : undefined}
        hint="Whole-machine CPU use, and the share used by the SiloForge process."
      />
      <StatCard
        label="Memory"
        value={cur ? fmtMb(cur.mem_used_mb) : '—'}
        sub={memTotal ? `of ${fmtMb(memTotal)} · SiloForge ${cur ? fmtMb(cur.proc_rss_mb) : '—'}` : undefined}
        hint="Machine RAM in use, and the SiloForge process's resident memory (RSS)."
      />
      <StatCard
        label="CPU capacity"
        value={logical ?? '—'}
        sub={[
          pool ? `${pool} work threads` : 'logical cores',
          cap.max_concurrent_jobs ? `${cap.max_concurrent_jobs} job slots` : '',
        ]
          .filter(Boolean)
          .join(' · ')}
        hint="Logical cores on this machine, and how many threads heavy jobs may use."
      />
      <StatCard
        label="Disk free"
        value={fmtMb(info.disk_free_mb ?? cur?.disk_free_mb ?? null)}
        sub={info.db_size_bytes ? `database ${fmtBytes(info.db_size_bytes)}` : undefined}
        hint="Free space on the drive that holds the SiloForge data folder."
      />
      <StatCard
        label="Jobs running"
        value={jobs.running}
        tone={jobs.running > 0 ? 'accent' : undefined}
        hint="Jobs being worked on right now."
      />
      <StatCard
        label="Jobs queued"
        value={jobs.queued}
        sub={jobs.failed_24h !== undefined ? `${jobs.failed_24h} failed in 24h` : undefined}
        tone={jobs.failed_24h ? 'warn' : undefined}
        hint="Jobs waiting for a free slot (heavy jobs run one per site)."
      />
    </div>
  );
}

function ErrorsCard() {
  const toast = useToast();
  const [rows, setRows] = useState<ErrorRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [showResolved, setShowResolved] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setRows(await fetchErrors());
      setError(null);
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(v2ErrorMessage(e, 'Failed to load errors'));
      setRows([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function resolve(fp: string) {
    setBusy(fp);
    try {
      await resolveError(fp);
      toast.push('Marked resolved', 'success');
      await load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Could not resolve'), 'error');
    } finally {
      setBusy(null);
    }
  }

  const visible = (rows ?? [])
    .filter((r) => showResolved || !r.resolved_at)
    .sort((a, b) => (b.last_seen > a.last_seen ? 1 : b.last_seen < a.last_seen ? -1 : 0));

  return (
    <div className="card">
      <div className="table-toolbar" style={{ marginBottom: 12 }}>
        <div className="card-title" style={{ marginBottom: 0 }}>
          Recent errors
        </div>
        <div className="spacer" />
        <label className="muted" style={{ display: 'inline-flex', gap: 6, alignItems: 'center', fontSize: 13 }}>
          <input type="checkbox" checked={showResolved} onChange={(e) => setShowResolved(e.target.checked)} /> Show resolved
        </label>
        <button className="btn btn-sm" onClick={() => void load()}>
          Refresh
        </button>
      </div>
      {unavailable ? (
        <FeatureUnavailable feature="Error tracking" />
      ) : rows === null ? (
        <Skeleton rows={3} />
      ) : (
        <>
          {error && <div className="error-box" style={{ marginBottom: 10 }}>{error}</div>}
          {visible.length === 0 ? (
            <div className="muted">No {showResolved ? '' : 'open '}errors. </div>
          ) : (
            <div className="tbl-wrap">
              <table className="tbl">
                <thead>
                  <tr>
                    <th>Source</th>
                    <th>Message</th>
                    <th className="num">Count</th>
                    <th>Last seen</th>
                    <th>First seen</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {visible.map((r) => (
                    <tr key={r.fingerprint} style={r.resolved_at ? { opacity: 0.55 } : undefined}>
                      <td className="mono">{r.source}</td>
                      <td style={{ maxWidth: 420, wordBreak: 'break-word' }} title={typeof r.context === 'string' ? r.context : JSON.stringify(r.context ?? {})}>
                        {r.message}
                      </td>
                      <td className="num">{r.count}</td>
                      <td className="muted">{fmtDate(r.last_seen)}</td>
                      <td className="muted">{fmtDate(r.first_seen)}</td>
                      <td>
                        {r.resolved_at ? (
                          <span className="faint" style={{ fontSize: 12 }}>
                            resolved
                          </span>
                        ) : (
                          <button
                            className="btn btn-sm"
                            disabled={busy === r.fingerprint}
                            onClick={() => void resolve(r.fingerprint)}
                            data-hint="Mark this error as handled. It reopens if it happens again."
                          >
                            Resolve
                          </button>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
    </div>
  );
}

/** Admin-only server monitor: version, capacity, live CPU/RAM, history charts, job queue, errors. */
export function SystemPage() {
  const { user } = useUser();
  const [info, setInfo] = useState<SystemInfo | null>(null);
  const [infoError, setInfoError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [win, setWin] = useState<MetricsWindow>('1h');
  const [samples, setSamples] = useState<MetricSample[] | null>(null);
  const [metricsError, setMetricsError] = useState<string | null>(null);

  // Live cards: poll every 5 s.
  useEffect(() => {
    let alive = true;
    let timer: number | undefined;
    const tick = async () => {
      try {
        const s = await fetchSystem();
        if (!alive) return;
        setInfo(s);
        setInfoError(null);
      } catch (e) {
        if (!alive) return;
        if (isUnavailable(e)) {
          setUnavailable(true);
          return;
        }
        setInfoError(v2ErrorMessage(e, 'Failed to load system status'));
      }
      if (alive) timer = window.setTimeout(() => void tick(), 5000);
    };
    void tick();
    return () => {
      alive = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, []);

  // Charts: reload on window change, refresh every 30 s (1h) or 5 min.
  useEffect(() => {
    let alive = true;
    let timer: number | undefined;
    setSamples(null);
    const tick = async () => {
      try {
        const res = await fetchSystemMetrics(win);
        if (!alive) return;
        setSamples(res.samples);
        setMetricsError(null);
      } catch (e) {
        if (!alive) return;
        setSamples([]);
        setMetricsError(v2ErrorMessage(e, 'Failed to load metrics'));
      }
      if (alive) timer = window.setTimeout(() => void tick(), win === '1h' ? 30000 : 300000);
    };
    void tick();
    return () => {
      alive = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [win]);

  const charts = useMemo(() => {
    const s = samples ?? [];
    const memTotal = s.length ? Math.max(...s.map((x) => x.mem_total_mb || 0)) : 0;
    return {
      cpu: series(s, (x) => x.cpu_pct),
      proc: series(s, (x) => x.proc_cpu_pct),
      mem: series(s, (x) => x.mem_used_mb),
      rss: series(s, (x) => x.proc_rss_mb),
      memTotal: memTotal || undefined,
    };
  }, [samples]);

  if (user && user.role !== 'admin') {
    return <EmptyState title="Admins only" hint="The System page is restricted to administrators." />;
  }

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>System</h1>
          <div className="page-sub">Server health: capacity, CPU and memory over time, the job queue and recent errors.</div>
        </div>
      </div>

      {unavailable ? (
        <div className="card">
          <FeatureUnavailable feature="The System monitor" />
        </div>
      ) : (
        <>
          {infoError && <div className="error-box" style={{ marginBottom: 14 }}>{infoError}</div>}
          {info ? <SystemCards info={info} /> : !infoError && <Skeleton rows={3} />}

          <div className="table-toolbar" style={{ marginBottom: 12 }}>
            <div className="card-title" style={{ marginBottom: 0 }}>
              Resource history
            </div>
            <div className="spacer" />
            <div className="tabs" style={{ marginBottom: 0, borderBottom: 'none' }} role="tablist" aria-label="Time window">
              {WINDOWS.map((w) => (
                <button key={w.id} type="button" role="tab" aria-selected={win === w.id} className={`tab${win === w.id ? ' active' : ''}`} onClick={() => setWin(w.id)}>
                  {w.label}
                </button>
              ))}
            </div>
          </div>
          {metricsError && <div className="error-box" style={{ marginBottom: 14 }}>{metricsError}</div>}
          {samples === null ? (
            <Skeleton rows={4} />
          ) : (
            <div className="chart-grid">
              <AreaChart title="Machine CPU" points={charts.cpu} max={100} format={pct} hint="Whole-machine CPU use (%)." />
              <AreaChart title="SiloForge CPU" points={charts.proc} max={100} format={pct} color="var(--teal)" hint="CPU used by the SiloForge process (% of total capacity)." />
              <AreaChart title="Machine RAM" points={charts.mem} max={charts.memTotal} format={mb} color="var(--purple)" hint="Memory in use on the machine." />
              <AreaChart title="SiloForge RSS" points={charts.rss} format={mb} color="var(--amber)" hint="Resident memory of the SiloForge process." />
            </div>
          )}

          <div className="card" style={{ marginTop: 14 }}>
            <div className="table-toolbar">
              <div className="card-title" style={{ marginBottom: 0 }}>
                Job queue
              </div>
              <span className="muted">
                {info ? `${jobCounts(info).running} running · ${jobCounts(info).queued} queued` : '—'}
              </span>
              <div className="spacer" />
              <Link className="btn btn-sm" to="/jobs">
                Open all jobs
              </Link>
              <Link className="btn btn-sm" to="/jobs?status=failed">
                Failed jobs
              </Link>
            </div>
          </div>

          <ErrorsCard />
        </>
      )}
    </div>
  );
}
