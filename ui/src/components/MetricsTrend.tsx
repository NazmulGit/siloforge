// Metric trends (checklist F4) — deltas against the previous snapshot plus a
// dependency-free inline-SVG sparkline over the last N snapshots.
//
// DIRECTION RULE: colour comes from `improved`, which the API computes from a
// per-metric direction table. Never colour by the sign of the delta — "orphans
// -4" is green and "broken_links +4" is red, and only the API knows which is
// which.
import { useCallback, useEffect, useMemo, useState } from 'react';
import { ApiError, fetchMetricsReport } from '../api';
import { fmtDate } from '../format';
import {
  METRIC_LABELS,
  type MetricDelta,
  type MetricKey,
  type MetricsReport,
  type SiteMetricsSnapshot,
} from '../types';
import { EmptyState, Skeleton } from './Skeleton';

const TREND_LIMIT = 30;
const SPARK_W = 640;
const SPARK_H = 96;
const SPARK_PAD = 8;

function labelFor(key: MetricKey): string {
  return METRIC_LABELS[key] ?? String(key).replace(/_/g, ' ');
}

function fmtNum(v: number | null | undefined): string {
  if (v === null || v === undefined) return '—';
  return Number.isInteger(v) ? v.toLocaleString() : v.toFixed(2);
}

/** Arrow shows DIRECTION of movement; the colour class shows whether it was good. */
function DeltaChip({ d }: { d: MetricDelta }) {
  if (d.delta === null || d.delta === 0) {
    return <span className="delta-chip delta-flat">no change</span>;
  }
  const arrow = d.delta > 0 ? '▲' : '▼';
  const tone = d.improved === true ? 'delta-good' : d.improved === false ? 'delta-bad' : 'delta-flat';
  const pct = d.pct === null || d.pct === undefined ? null : `${d.pct > 0 ? '+' : ''}${d.pct}%`;
  return (
    <span className={`delta-chip ${tone}`}>
      {arrow} {d.delta > 0 ? '+' : ''}
      {fmtNum(d.delta)}
      {pct && <span className="delta-pct">{pct}</span>}
    </span>
  );
}

function DeltaCard({ d }: { d: MetricDelta }) {
  const verdict =
    d.improved === true ? 'improved since the previous run' : d.improved === false ? 'got worse since the previous run' : 'moved, but neither direction is inherently better';
  return (
    <div
      className="stat-card delta-card"
      data-hint={`${labelFor(d.key)}: ${fmtNum(d.previous)} → ${fmtNum(d.current)} — ${verdict}.`}
    >
      <div className="stat-label">{labelFor(d.key)}</div>
      <div className="stat-value">{fmtNum(d.current)}</div>
      <div className="stat-sub">
        <DeltaChip d={d} />
        <span className="faint"> from {fmtNum(d.previous)}</span>
      </div>
    </div>
  );
}

interface SparkPoint {
  x: number;
  y: number;
  value: number;
  at: string;
}

function Sparkline({ snapshots, metric }: { snapshots: SiteMetricsSnapshot[]; metric: MetricKey }) {
  const series = useMemo(() => {
    const vals: Array<{ value: number; at: string }> = [];
    for (const s of snapshots) {
      const v = s.metrics?.[metric];
      if (typeof v === 'number' && Number.isFinite(v)) vals.push({ value: v, at: s.captured_at });
    }
    return vals;
  }, [snapshots, metric]);

  if (series.length < 2) {
    return (
      <div className="muted" style={{ padding: '18px 0' }}>
        Not enough history for a trend yet — {series.length === 0 ? 'no snapshot' : 'one snapshot'} carries{' '}
        {labelFor(metric).toLowerCase()}. Run the audit at least twice.
      </div>
    );
  }

  const values = series.map((s) => s.value);
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const innerW = SPARK_W - SPARK_PAD * 2;
  const innerH = SPARK_H - SPARK_PAD * 2;

  const points: SparkPoint[] = series.map((s, i) => ({
    x: SPARK_PAD + (i / (series.length - 1)) * innerW,
    y: SPARK_PAD + innerH - ((s.value - min) / span) * innerH,
    value: s.value,
    at: s.at,
  }));

  const line = points.map((p) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`).join(' ');
  const area = `${SPARK_PAD},${SPARK_H - SPARK_PAD} ${line} ${(SPARK_W - SPARK_PAD).toFixed(1)},${SPARK_H - SPARK_PAD}`;
  const last = points[points.length - 1];
  const first = points[0];

  return (
    <div className="spark-wrap">
      <svg
        className="spark"
        viewBox={`0 0 ${SPARK_W} ${SPARK_H}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`${labelFor(metric)} over the last ${series.length} snapshots: ${fmtNum(first.value)} to ${fmtNum(last.value)}`}
      >
        <polygon className="spark-area" points={area} />
        <polyline className="spark-line" points={line} />
        {points.map((p) => (
          <circle key={`${p.at}-${p.x}`} className="spark-dot" cx={p.x} cy={p.y} r={2.4}>
            <title>{`${fmtDate(p.at)} — ${fmtNum(p.value)}`}</title>
          </circle>
        ))}
      </svg>
      <div className="spark-legend">
        <span>
          min <b>{fmtNum(min)}</b>
        </span>
        <span>
          max <b>{fmtNum(max)}</b>
        </span>
        <span>
          latest <b>{fmtNum(last.value)}</b>
        </span>
        <span className="faint">
          {series.length} snapshots · {fmtDate(first.at)} → {fmtDate(last.at)}
        </span>
      </div>
    </div>
  );
}

export function MetricsTrend({ siteId }: { siteId: string }) {
  const [report, setReport] = useState<MetricsReport | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [metric, setMetric] = useState<MetricKey>('health_score');

  const load = useCallback(async () => {
    setError(null);
    try {
      setReport(await fetchMetricsReport(siteId, TREND_LIMIT));
    } catch (e) {
      const msg =
        e instanceof ApiError && (e.status === 404 || e.status === 403)
          ? 'Metric history is not available for this site yet.'
          : e instanceof Error
            ? e.message
            : 'Failed to load metric history';
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

  // Metrics that actually appear in the history — no empty dropdown entries.
  const availableMetrics = useMemo(() => {
    const keys = new Set<MetricKey>();
    for (const s of report?.trend ?? []) {
      for (const [k, v] of Object.entries(s.metrics ?? {})) {
        if (typeof v === 'number') keys.add(k as MetricKey);
      }
    }
    for (const d of report?.deltas ?? []) keys.add(d.key);
    return [...keys].sort((a, b) => labelFor(a).localeCompare(labelFor(b)));
  }, [report]);

  useEffect(() => {
    if (availableMetrics.length === 0) return;
    if (!availableMetrics.includes(metric)) setMetric(availableMetrics[0]);
  }, [availableMetrics, metric]);

  const moved = useMemo(
    () => (report?.deltas ?? []).filter((d) => d.delta !== null && d.delta !== 0),
    [report],
  );
  const unchanged = useMemo(
    () => (report?.deltas ?? []).filter((d) => d.delta === null || d.delta === 0),
    [report],
  );

  if (loading) return <Skeleton rows={8} />;
  if (error) return <div className="error-box">{error}</div>;
  if (!report || (report.latest === null && report.trend.length === 0)) {
    return (
      <EmptyState
        title="No metric snapshots yet"
        hint="Snapshots are captured by the audit job — run one from the Schedules panel to start the trend."
      />
    );
  }

  return (
    <>
      <div className="card">
        <div className="table-toolbar" style={{ marginBottom: 12 }}>
          <div className="card-title" style={{ marginBottom: 0 }}>
            Since the previous run
          </div>
          <div className="spacer" />
          {report.latest && (
            <span className="muted">
              <span className="chip chip-mode">{report.latest.source}</span> captured{' '}
              {fmtDate(report.latest.captured_at)}
            </span>
          )}
        </div>

        <div className="info-note" style={{ marginBottom: 14 }}>
          <span className="info-note-icon" aria-hidden="true">
            ℹ
          </span>
          <span className="info-note-text">
            Green means <b>better</b>, not bigger — fewer orphans and fewer broken links are wins even
            though the number goes down.
          </span>
        </div>

        {moved.length === 0 ? (
          <div className="muted">
            Nothing changed since the previous snapshot
            {unchanged.length > 0 && <> — {unchanged.length} metrics held steady</>}.
          </div>
        ) : (
          <div className="dash-grid dash-grid-4">
            {moved.map((d) => (
              <DeltaCard key={String(d.key)} d={d} />
            ))}
          </div>
        )}
      </div>

      <div className="card">
        <div className="table-toolbar" style={{ marginBottom: 12 }}>
          <div className="card-title" style={{ marginBottom: 0 }}>
            Trend
          </div>
          <div className="spacer" />
          <label className="field trend-picker">
            Metric
            <select
              className="input"
              value={metric}
              onChange={(e) => setMetric(e.target.value as MetricKey)}
              data-hint="Pick which metric the sparkline plots across your recent snapshots."
            >
              {availableMetrics.map((k) => (
                <option key={String(k)} value={String(k)}>
                  {labelFor(k)}
                </option>
              ))}
            </select>
          </label>
        </div>
        {availableMetrics.length === 0 ? (
          <div className="muted">No numeric metrics recorded yet.</div>
        ) : (
          <Sparkline snapshots={report.trend} metric={metric} />
        )}
      </div>
    </>
  );
}
