import { useMemo } from 'react';

export interface ChartPoint {
  /** Epoch milliseconds. */
  t: number;
  v: number;
}

interface AreaChartProps {
  title: string;
  points: ChartPoint[];
  /** Fixed y-axis maximum (e.g. 100 for percentages). Defaults to the data max. */
  max?: number;
  format: (v: number) => string;
  color?: string;
  hint?: string;
}

const W = 600;
const H = 120;
const PAD_Y = 6;

function tick(t: number, span: number): string {
  const d = new Date(t);
  return span > 86_400_000
    ? d.toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })
    : d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
}

/** Small dependency-free SVG area chart (line + filled area) with min/avg/max readout. */
export function AreaChart({ title, points, max, format, color, hint }: AreaChartProps) {
  const geo = useMemo(() => {
    if (points.length === 0) return null;
    const sorted = [...points].sort((a, b) => a.t - b.t);
    const t0 = sorted[0].t;
    const t1 = sorted[sorted.length - 1].t;
    const span = Math.max(1, t1 - t0);
    const vals = sorted.map((p) => p.v);
    const dataMax = Math.max(...vals);
    const top = Math.max(max ?? dataMax * 1.1, 1e-9);
    const x = (t: number) => (sorted.length === 1 ? W / 2 : ((t - t0) / span) * W);
    const y = (v: number) => H - PAD_Y - (Math.min(Math.max(v, 0), top) / top) * (H - 2 * PAD_Y);
    const line = sorted.map((p, i) => `${i === 0 ? 'M' : 'L'}${x(p.t).toFixed(1)},${y(p.v).toFixed(1)}`).join(' ');
    const area = `${line} L${x(t1).toFixed(1)},${H} L${x(t0).toFixed(1)},${H} Z`;
    const avg = vals.reduce((a, b) => a + b, 0) / vals.length;
    return {
      line,
      area,
      last: vals[vals.length - 1],
      min: Math.min(...vals),
      avg,
      max: dataMax,
      from: tick(t0, span),
      to: tick(t1, span),
    };
  }, [points, max]);

  const stroke = color ?? 'var(--accent)';
  return (
    <div className="card chart-card" data-hint={hint}>
      <div className="chart-head">
        <span className="card-title" style={{ marginBottom: 0 }}>
          {title}
        </span>
        <span className="chart-last">{geo ? format(geo.last) : '—'}</span>
      </div>
      {geo ? (
        <>
          <svg className="spark" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="img" aria-label={`${title} chart`}>
            <path d={geo.area} style={{ fill: stroke, opacity: 0.12 }} />
            <path className="spark-line" d={geo.line} style={{ stroke }} />
          </svg>
          <div className="chart-foot">
            <span>
              min {format(geo.min)} · avg {format(geo.avg)} · max {format(geo.max)}
            </span>
            <span>
              {geo.from} – {geo.to}
            </span>
          </div>
        </>
      ) : (
        <div className="muted" style={{ fontSize: 13, padding: '30px 0', textAlign: 'center' }}>
          No samples yet.
        </div>
      )}
    </div>
  );
}
