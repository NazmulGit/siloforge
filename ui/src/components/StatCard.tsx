interface StatCardProps {
  label: string;
  value: string | number;
  sub?: string;
  tone?: 'good' | 'warn' | 'bad' | 'accent';
  /** Educational hover hint explaining what the metric means. */
  hint?: string;
}

export function StatCard({ label, value, sub, tone, hint }: StatCardProps) {
  return (
    <div className="stat-card" data-hint={hint}>
      <div className="stat-label">{label}</div>
      <div className={`stat-value${tone ? ` tone-${tone}` : ''}`}>{value}</div>
      {sub && <div className="stat-sub">{sub}</div>}
    </div>
  );
}
