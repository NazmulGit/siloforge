import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { fmtTokens, fmtUptime } from '../../format';
import { fetchAdminOverview, type AdminOverview as Overview } from '../../saas';
import { Skeleton } from '../Skeleton';
import { StatCard } from '../StatCard';

const MODE_LABEL: Record<string, string> = { closed: 'Closed', invite_only: 'Invite only', open: 'Open' };

export function AdminOverview() {
  const [o, setO] = useState<Overview | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    const load = () =>
      fetchAdminOverview()
        .then((r) => alive && setO(r))
        .catch((e: unknown) => alive && setError(e instanceof Error ? e.message : 'Could not load the overview'));
    void load();
    const t = window.setInterval(() => void load(), 30_000);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, []);

  if (error && !o) return <div className="error-box">{error}</div>;
  if (!o) return <Skeleton rows={6} />;

  return (
    <>
      <div className="stat-grid">
        <StatCard label="Users" value={o.users.total.toLocaleString()} sub={`${o.users.admins} platform admins, ${o.users.disabled} disabled`} />
        <StatCard
          label="Unverified email"
          value={o.users.unverified.toLocaleString()}
          tone={o.users.unverified > 0 ? 'warn' : undefined}
          sub={`${o.users.with_2fa} with two-factor sign-in`}
        />
        <StatCard
          label="Workspaces"
          value={o.workspaces.total.toLocaleString()}
          sub={`${o.workspaces.suspended} suspended, ${o.workspaces.deleted} deleted`}
          tone={o.workspaces.suspended > 0 ? 'warn' : undefined}
        />
        <StatCard label="Sites" value={o.sites.toLocaleString()} sub={`${o.posts.toLocaleString()} posts`} />
        <StatCard label="Runs today" value={o.runs_today.toLocaleString()} sub={`${o.health.jobs.running} running, ${o.health.jobs.queued} queued`} />
        <StatCard label="AI tokens this month" value={fmtTokens(o.ai_tokens_month)} />
        <StatCard label="Open errors" value={o.errors_open.toLocaleString()} tone={o.errors_open > 0 ? 'bad' : 'good'} />
        <StatCard
          label="Mail"
          value={o.mail.configured ? 'SMTP' : 'Manual'}
          sub={o.mail.outbox_pending > 0 ? `${o.mail.outbox_pending} links waiting in the outbox` : 'outbox empty'}
          tone={!o.mail.configured && o.mail.outbox_pending > 0 ? 'warn' : undefined}
        />
      </div>
      <div className="card">
        <dl className="kv-list">
          <div>
            <dt>Registration</dt>
            <dd>
              {MODE_LABEL[o.registration_mode] ?? o.registration_mode} <Link to="/admin?tab=registration">change</Link>
            </dd>
          </div>
          <div>
            <dt>Version</dt>
            <dd>{o.health.version}</dd>
          </div>
          <div>
            <dt>Uptime</dt>
            <dd>{fmtUptime(o.health.uptime_secs)}</dd>
          </div>
          <div>
            <dt>Server health</dt>
            <dd>
              <Link to="/system">System page</Link>
            </dd>
          </div>
        </dl>
      </div>
    </>
  );
}
