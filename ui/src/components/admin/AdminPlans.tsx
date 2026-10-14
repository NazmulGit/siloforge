import { useCallback, useEffect, useState } from 'react';
import { ApiError } from '../../api';
import { createPlan, deletePlan, fetchPlans, LIMIT_LABELS, NUMERIC_LIMITS, patchPlan, type Plan, type PlanLimits } from '../../saas';
import { useToast } from '../../toast';
import { Modal } from '../Modal';
import { Skeleton } from '../Skeleton';
import { Toggle } from '../Toggle';
import { errText } from './common';

const EMPTY_LIMITS: PlanLimits = {
  sites: 1,
  posts_per_site: 500,
  link_runs_per_month: 5,
  ai_tokens_per_month: 100000,
  members: 1,
  api_tokens: 1,
  schedules: 1,
  auto_linker: false,
  mcp: false,
};

function PlanEditor({ plan, onClose, onSaved }: { plan: Plan | null; onClose: () => void; onSaved: () => void }) {
  const toast = useToast();
  const [id, setId] = useState(plan?.id ?? '');
  const [name, setName] = useState(plan?.name ?? '');
  const [price, setPrice] = useState(plan?.price_label ?? '');
  const [pub, setPub] = useState(plan?.public ?? true);
  const [sort, setSort] = useState(plan?.sort ?? 50);
  const [limits, setLimits] = useState<PlanLimits>({ ...EMPTY_LIMITS, ...(plan?.limits ?? {}) });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function save() {
    setError(null);
    if (!name.trim()) return setError('Give the plan a name.');
    if (!plan && !/^[a-z0-9][a-z0-9_-]{0,31}$/.test(id)) return setError('Id: lowercase letters, digits, dashes or underscores (for example "team").');
    setBusy(true);
    try {
      const body = { name: name.trim(), price_label: price.trim() || null, public: pub, sort, limits };
      if (plan) await patchPlan(plan.id, body);
      else await createPlan({ id, ...body });
      toast.push(plan ? 'Plan saved' : 'Plan created', 'success');
      onSaved();
    } catch (e) {
      setError(errText(e, 'Could not save the plan'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      title={plan ? `Edit ${plan.name}` : 'New plan'}
      wide
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={busy} onClick={() => void save()}>
            {busy ? 'Saving…' : plan ? 'Save plan' : 'Create plan'}
          </button>
        </>
      }
    >
      {error && <div className="error-box">{error}</div>}
      <div className="plan-editor">
        <label className="field">
          Id
          <input className="mono" value={id} disabled={!!plan} onChange={(e) => setId(e.target.value.toLowerCase())} placeholder="team" />
        </label>
        <label className="field">
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Team" />
        </label>
        <label className="field">
          Price label
          <input value={price} onChange={(e) => setPrice(e.target.value)} placeholder="$49 / month" />
        </label>
        <label className="field">
          Sort order
          <input type="number" value={sort} onChange={(e) => setSort(Number(e.target.value) || 0)} />
        </label>
        <label className="check-line">
          <input type="checkbox" checked={pub} onChange={(e) => setPub(e.target.checked)} /> Public (shown to users)
        </label>
      </div>
      <div className="card-title" style={{ marginTop: 14 }}>
        Limits
      </div>
      <div className="limit-grid">
        {NUMERIC_LIMITS.map((k) => {
          const v = limits[k];
          return (
            <div key={k} className="limit-cell">
              <label className="field">
                {LIMIT_LABELS[k]}
                <input
                  type="number"
                  min={0}
                  value={v ?? ''}
                  disabled={v === null}
                  placeholder="unlimited"
                  onChange={(e) => setLimits((l) => ({ ...l, [k]: Math.max(0, Math.floor(Number(e.target.value) || 0)) }))}
                />
              </label>
              <label className="check-line faint">
                <input type="checkbox" checked={v === null} onChange={(e) => setLimits((l) => ({ ...l, [k]: e.target.checked ? null : (EMPTY_LIMITS[k] ?? 0) }))} />
                Unlimited
              </label>
            </div>
          );
        })}
        <div className="limit-cell">
          <span className="field">Auto-Linker</span>
          <Toggle checked={limits.auto_linker} onChange={(v) => setLimits((l) => ({ ...l, auto_linker: v }))} label="Auto-Linker" />
        </div>
        <div className="limit-cell">
          <span className="field">MCP access</span>
          <Toggle checked={limits.mcp} onChange={(v) => setLimits((l) => ({ ...l, mcp: v }))} label="MCP access" />
        </div>
      </div>
    </Modal>
  );
}

function fmt(v: number | null): string {
  return v === null ? 'no limit' : v.toLocaleString();
}

export function AdminPlans() {
  const toast = useToast();
  const [plans, setPlans] = useState<Plan[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Plan | 'new' | null>(null);

  const load = useCallback(async () => {
    try {
      setPlans((await fetchPlans()).sort((a, b) => a.sort - b.sort || a.name.localeCompare(b.name)));
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load plans'));
      setPlans([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function remove(p: Plan) {
    if (!window.confirm(`Delete the ${p.name} plan?`)) return;
    try {
      await deletePlan(p.id);
      toast.push('Plan deleted', 'success');
      await load();
    } catch (e) {
      toast.push(e instanceof ApiError && e.status === 409 ? `${p.name} is still assigned to a workspace. Move those workspaces to another plan first.` : errText(e, 'Could not delete'), 'error');
    }
  }

  return (
    <div className="card">
      <div className="table-toolbar">
        <p className="muted card-lead" style={{ margin: 0 }}>
          Limits apply per workspace. Empty means unlimited. A plan that a workspace uses cannot be deleted.
        </p>
        <div className="spacer" />
        <button className="btn btn-primary btn-sm" onClick={() => setEditing('new')}>
          New plan
        </button>
      </div>
      {error && <div className="error-box">{error}</div>}
      {plans === null ? (
        <Skeleton rows={4} />
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Plan</th>
                {NUMERIC_LIMITS.map((k) => (
                  <th key={k} className="num">
                    {LIMIT_LABELS[k]}
                  </th>
                ))}
                <th>Includes</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {plans.map((p) => (
                <tr key={p.id}>
                  <td>
                    <div>
                      {p.name} {!p.public && <span className="chip chip-reverted">internal</span>}
                    </div>
                    <div className="faint field-note">{p.price_label || p.id}</div>
                  </td>
                  {NUMERIC_LIMITS.map((k) => (
                    <td key={k} className="num">
                      {fmt(p.limits[k])}
                    </td>
                  ))}
                  <td className="muted">{[p.limits.auto_linker && 'Auto-Linker', p.limits.mcp && 'MCP'].filter(Boolean).join(', ') || '—'}</td>
                  <td>
                    <div className="row-actions">
                      <button className="btn btn-sm" onClick={() => setEditing(p)}>
                        Edit
                      </button>
                      <button className="btn btn-sm btn-danger" onClick={() => void remove(p)}>
                        Delete
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {editing && (
        <PlanEditor
          plan={editing === 'new' ? null : editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void load();
          }}
        />
      )}
    </div>
  );
}
