import { Fragment, useCallback, useEffect, useState } from 'react';
import { fmtDate } from '../../format';
import {
  adminWorkspaceAction,
  deleteAdminWorkspace,
  fetchAdminWorkspace,
  fetchAdminWorkspaces,
  fetchPlans,
  patchAdminWorkspace,
  type AdminWorkspace,
  type MemberRow,
  type Plan,
} from '../../saas';
import { useToast } from '../../toast';
import { useWorkspace } from '../../workspaces';
import { Skeleton } from '../Skeleton';
import { UsageBars } from '../workspace/UsageBars';
import { errText, Pager, useDebounced } from './common';

const LIMIT = 50;

function memberCount(w: AdminWorkspace): number | null {
  if (typeof w.members === 'number') return w.members;
  if (Array.isArray(w.members)) return w.members.length;
  return w.usage?.members ?? null;
}

function Detail({ id }: { id: string }) {
  const [d, setD] = useState<(AdminWorkspace & { members?: MemberRow[] }) | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    fetchAdminWorkspace(id)
      .then(setD)
      .catch((e: unknown) => setError(errText(e, 'Could not load')));
  }, [id]);
  if (error) return <div className="error-box">{error}</div>;
  if (!d) return <Skeleton rows={3} />;
  const members = Array.isArray(d.members) ? d.members : [];
  return (
    <div className="ws-detail">
      <div>
        <div className="card-title">Usage vs limits</div>
        {d.limits && d.usage ? <UsageBars limits={d.limits} usage={d.usage} compact /> : <div className="muted">No usage data.</div>}
      </div>
      <div>
        <div className="card-title">Members</div>
        {members.length === 0 ? (
          <div className="muted">No members.</div>
        ) : (
          <ul className="plain-list">
            {members.map((m) => (
              <li key={m.user_id}>
                {m.name || m.email} <span className="faint">{m.role}</span>
              </li>
            ))}
          </ul>
        )}
        {d.purge_after && <div className="tone-warn field-note">Purged on {fmtDate(d.purge_after)} unless restored.</div>}
      </div>
    </div>
  );
}

export function AdminWorkspaces() {
  const toast = useToast();
  const { startViewAs, reload: reloadMine } = useWorkspace();
  const [input, q, setQ] = useDebounced();
  const [status, setStatus] = useState('');
  const [offset, setOffset] = useState(0);
  const [rows, setRows] = useState<AdminWorkspace[] | null>(null);
  const [total, setTotal] = useState(0);
  const [plans, setPlans] = useState<Plan[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const r = await fetchAdminWorkspaces({ q: q || undefined, status: status || undefined, limit: LIMIT, offset });
      setRows(r.workspaces);
      setTotal(r.total);
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load workspaces'));
      setRows([]);
    }
  }, [q, status, offset]);

  useEffect(() => {
    void load();
  }, [load]);
  useEffect(() => setOffset(0), [q, status]);
  useEffect(() => {
    fetchPlans()
      .then(setPlans)
      .catch(() => setPlans([]));
  }, []);

  async function run(w: AdminWorkspace, fn: () => Promise<unknown>, done: string) {
    setBusy(w.id);
    try {
      await fn();
      toast.push(done, 'success');
      await load();
      void reloadMine();
    } catch (e) {
      toast.push(errText(e, 'Action failed'), 'error');
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="card">
      <div className="table-toolbar">
        <input className="input" type="search" placeholder="Search by name or slug" aria-label="Search workspaces" value={input} onChange={(e) => setQ(e.target.value)} />
        <select aria-label="Status" value={status} onChange={(e) => setStatus(e.target.value)}>
          <option value="">Any status</option>
          <option value="active">Active</option>
          <option value="suspended">Suspended</option>
          <option value="deleted">Deleted</option>
        </select>
      </div>
      {error && <div className="error-box">{error}</div>}
      {rows === null ? (
        <Skeleton rows={6} />
      ) : rows.length === 0 ? (
        <div className="empty">
          <div className="empty-title">No workspaces match</div>
          <div className="empty-hint">Clear the search or the status filter.</div>
        </div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Workspace</th>
                <th>Owner</th>
                <th>Plan</th>
                <th className="num">Members</th>
                <th className="num">Sites</th>
                <th>Status</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rows.map((w) => {
                const deleted = !!w.deleted_at || w.status === 'deleted';
                const mc = memberCount(w);
                return (
                  <Fragment key={w.id}>
                    <tr>
                      <td>
                        <button className="link-btn" aria-expanded={open === w.id} onClick={() => setOpen(open === w.id ? null : w.id)}>
                          {w.name}
                        </button>
                        <div className="faint field-note mono-sm">{w.slug}</div>
                      </td>
                      <td className="muted">{w.owner?.email ?? '—'}</td>
                      <td>
                        <select
                          aria-label={`Plan of ${w.name}`}
                          value={w.plan?.id ?? ''}
                          disabled={busy === w.id || deleted}
                          onChange={(e) =>
                            void run(w, () => patchAdminWorkspace(w.id, { plan_id: e.target.value }), `${w.name} moved to ${plans.find((p) => p.id === e.target.value)?.name ?? e.target.value}`)
                          }
                        >
                          {!plans.some((p) => p.id === w.plan?.id) && <option value={w.plan?.id ?? ''}>{w.plan?.name ?? '—'}</option>}
                          {plans.map((p) => (
                            <option key={p.id} value={p.id}>
                              {p.name}
                            </option>
                          ))}
                        </select>
                      </td>
                      <td className="num">{mc ?? '—'}</td>
                      <td className="num">{w.sites ?? w.usage?.sites ?? '—'}</td>
                      <td>
                        <span className={`chip chip-${deleted ? 'failed' : w.status === 'suspended' ? 'reverted' : 'done'}`}>{deleted ? 'deleted' : w.status}</span>
                      </td>
                      <td>
                        <div className="row-actions">
                          {!deleted && (
                            <button className="btn btn-sm" onClick={() => startViewAs({ id: w.id, name: w.name })} data-hint="Open this workspace read-only, as its members see it. Every request is logged.">
                              View as
                            </button>
                          )}
                          {deleted ? (
                            <button className="btn btn-sm" disabled={busy === w.id} onClick={() => void run(w, () => adminWorkspaceAction(w.id, 'restore'), `${w.name} restored`)}>
                              Restore
                            </button>
                          ) : w.status === 'suspended' ? (
                            <button className="btn btn-sm" disabled={busy === w.id} onClick={() => void run(w, () => adminWorkspaceAction(w.id, 'resume'), `${w.name} resumed`)}>
                              Resume
                            </button>
                          ) : (
                            <button
                              className="btn btn-sm"
                              disabled={busy === w.id}
                              onClick={() =>
                                window.confirm(`Suspend ${w.name}? Its members can still look, but nothing runs or changes.`) &&
                                void run(w, () => adminWorkspaceAction(w.id, 'suspend'), `${w.name} suspended`)
                              }
                            >
                              Suspend
                            </button>
                          )}
                          {!deleted && !w.is_default && (
                            <button
                              className="btn btn-sm btn-danger"
                              disabled={busy === w.id}
                              onClick={() =>
                                window.confirm(`Delete ${w.name}? It disappears for its members now and is purged after 30 days unless restored.`) &&
                                void run(w, () => deleteAdminWorkspace(w.id), `${w.name} deleted`)
                              }
                            >
                              Delete
                            </button>
                          )}
                        </div>
                      </td>
                    </tr>
                    {open === w.id && (
                      <tr className="log-detail">
                        <td colSpan={7}>
                          <Detail id={w.id} />
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
      <Pager offset={offset} limit={LIMIT} total={total} onPage={setOffset} />
    </div>
  );
}
