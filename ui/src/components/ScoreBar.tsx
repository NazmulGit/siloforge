import { scoreTone } from '../format';

/** Renders a 0..1 composite score as a colored 0-100 number + mini bar. */
export function ScoreBar({ score }: { score: number }) {
  const pct = Math.round(Math.max(0, Math.min(1, score)) * 100);
  const tone = scoreTone(pct);
  return (
    <span className={`scorebar scorebar-${tone}`}>
      <span className="scorebar-num">{pct}</span>
      <span className="scorebar-track">
        <span className="scorebar-fill" style={{ width: `${pct}%` }} />
      </span>
    </span>
  );
}
