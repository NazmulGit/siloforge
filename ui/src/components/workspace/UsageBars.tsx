import { LIMIT_LABELS, NUMERIC_LIMITS, usageFor, type PlanLimits, type WorkspaceUsage } from '../../saas';

function pct(used: number, limit: number | null): number {
  if (limit === null) return 0;
  if (limit <= 0) return used > 0 ? 100 : 0;
  return Math.min(100, Math.round((used / limit) * 100));
}

/** Limits vs usage, one bar per numeric limit; null = unlimited (no bar fill, "no limit"). */
export function UsageBars({ limits, usage, compact }: { limits: PlanLimits; usage: WorkspaceUsage; compact?: boolean }) {
  return (
    <div className={`usage-bars${compact ? ' compact' : ''}`}>
      {NUMERIC_LIMITS.map((k) => {
        const used = usageFor(k, usage);
        const limit = limits[k];
        const p = pct(used, limit);
        const tone = limit === null ? 'free' : p >= 100 ? 'full' : p >= 80 ? 'near' : 'ok';
        return (
          <div key={k} className={`ubar ubar-${tone}`}>
            <div className="ubar-head">
              <span className="ubar-label">{LIMIT_LABELS[k]}</span>
              <span className="ubar-num">
                {used.toLocaleString()}
                <span className="faint"> / {limit === null ? 'no limit' : limit.toLocaleString()}</span>
              </span>
            </div>
            <div
              className="ubar-track"
              role="meter"
              aria-label={LIMIT_LABELS[k]}
              aria-valuemin={0}
              aria-valuemax={limit ?? undefined}
              aria-valuenow={used}
            >
              <span style={{ width: `${limit === null ? 0 : p}%` }} />
            </div>
            {k === 'posts_per_site' && !compact && <div className="ubar-note faint">largest site; {usage.posts_total.toLocaleString()} posts in total</div>}
            {k === 'members' && !compact && usage.pending_invites > 0 && (
              <div className="ubar-note faint">includes {usage.pending_invites} pending invitation{usage.pending_invites === 1 ? '' : 's'}</div>
            )}
          </div>
        );
      })}
      <div className="ubar-flags">
        <span className={`flag ${limits.auto_linker ? 'on' : 'off'}`}>Auto-Linker {limits.auto_linker ? 'included' : 'not included'}</span>
        <span className={`flag ${limits.mcp ? 'on' : 'off'}`}>MCP access {limits.mcp ? 'included' : 'not included'}</span>
      </div>
    </div>
  );
}
