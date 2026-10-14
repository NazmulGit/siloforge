// Workspace settings (v2.4 §5): profile, members & invites, plan & usage, API tokens + MCP
// policy (moved here from Settings → API & MCP) and the danger zone. Viewers read, admins
// manage, the owner (or a platform admin) gets the danger zone.

import { useCallback, useEffect, useState, type FormEvent } from 'react';
import { useNavigate, useSearchParams } from 'react-router-dom';
import { isPlatformAdmin } from '../api';
import { CopyButton } from '../components/CopyButton';
import { Skeleton } from '../components/Skeleton';
import { ApiMcpPanel } from '../components/settings/ApiMcpPanel';
import { UsageBars } from '../components/workspace/UsageBars';
import { HelpButton } from '../docs';
import { absoluteLink, fmtDate } from '../format';
import {
  createInvite,
  deleteWorkspace,
  fetchBilling,
  fetchInvites,
  fetchMembers,
  fetchUsageReport,
  fetchWorkspace,
  patchMember,
  patchWorkspace,
  removeMember,
  revokeInvite,
  roleAtLeast,
  transferWorkspace,
  type BillingInfo,
  type InviteRow,
  type MemberRole,
  type MemberRow,
  type UsageReport,
  type WorkspaceDetail,
} from '../saas';
import { useToast } from '../toast';
import { useUser } from '../user';
import { useWorkspace } from '../workspaces';

type TabId = 'profile' | 'members' | 'plan' | 'tokens' | 'danger';

const TABS: Array<{ id: TabId; label: string; hint: string }> = [
  { id: 'profile', label: 'Profile', hint: 'Name and URL slug of this workspace.' },
  { id: 'members', label: 'Members & invites', hint: 'Who can work in this workspace, with which role, and pending invitations.' },
  { id: 'plan', label: 'Plan & usage', hint: 'Your plan limits and what this workspace has used this month.' },
  { id: 'tokens', label: 'API tokens & MCP', hint: 'Tokens for Hermes, MCP clients and scripts in this workspace, and which MCP tools they may use.' },
  { id: 'danger', label: 'Danger zone', hint: 'Transfer ownership or delete this workspace. Owner only.' },
];

const MEMBER_ROLES: Array<{ id: MemberRole; label: string; hint: string }> = [
  { id: 'viewer', label: 'Viewer', hint: 'Sees everything, changes nothing.' },
  { id: 'member', label: 'Member', hint: 'Runs crawls and plans, approves and deploys links.' },
  { id: 'admin', label: 'Admin', hint: 'Also manages settings, members and tokens.' },
];

function errText(e: unknown, fallback: string): string {
  return e instanceof Error && e.message ? e.message : fallback;
}

// ---------------------------------------------------------------------------

function ProfileTab({ ws, canManage, onSaved }: { ws: WorkspaceDetail; canManage: boolean; onSaved: () => void }) {
  const toast = useToast();
  const [name, setName] = useState(ws.name);
  const [slug, setSlug] = useState(ws.slug);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    setName(ws.name);
    setSlug(ws.slug);
  }, [ws]);

  async function save(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    try {
      await patchWorkspace(ws.id, { name: name.trim(), slug: slug.trim() });
      toast.push('Workspace saved', 'success');
      onSaved();
    } catch (err) {
      toast.push(errText(err, 'Could not save the workspace'), 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="card">
      <div className="card-title">Profile</div>
      <form className="form-grid" onSubmit={(e) => void save(e)}>
        <label className="field">
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} disabled={!canManage} maxLength={80} />
        </label>
        <label className="field">
          Slug
          <input
            value={slug}
            onChange={(e) => setSlug(e.target.value.toLowerCase().replace(/[^a-z0-9-]/g, '-'))}
            disabled={!canManage}
            className="mono"
            maxLength={48}
          />
          <span className="field-note faint">Short id used in links and to confirm deletion. Lowercase letters, digits and dashes.</span>
        </label>
        <dl className="kv-list">
          <div>
            <dt>Plan</dt>
            <dd>{ws.plan?.name ?? '—'}</dd>
          </div>
          <div>
            <dt>Status</dt>
            <dd>{ws.status}</dd>
          </div>
          <div>
            <dt>Created</dt>
            <dd>{ws.created_at ? fmtDate(ws.created_at) : '—'}</dd>
          </div>
          <div>
            <dt>Id</dt>
            <dd className="mono-sm">{ws.id}</dd>
          </div>
        </dl>
        {canManage ? (
          <div className="form-actions">
            <button className="btn btn-primary" disabled={busy || !name.trim() || !slug.trim() || (name === ws.name && slug === ws.slug)}>
              {busy ? 'Saving…' : 'Save changes'}
            </button>
          </div>
        ) : (
          <div className="faint">Only workspace admins can change these.</div>
        )}
      </form>
    </div>
  );
}

// ---------------------------------------------------------------------------

function MembersTab({ ws, myRole, canManage }: { ws: WorkspaceDetail; myRole: string; canManage: boolean }) {
  const toast = useToast();
  const { user } = useUser();
  const navigate = useNavigate();
  const { reload: reloadWorkspaces } = useWorkspace();
  const [members, setMembers] = useState<MemberRow[] | null>(null);
  const [invites, setInvites] = useState<InviteRow[] | null>(null);
  const [links, setLinks] = useState<Record<string, string>>({});
  const [email, setEmail] = useState('');
  const [role, setRole] = useState<MemberRole>('member');
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const isOwnerish = myRole === 'owner' || myRole === 'platform_admin';

  const load = useCallback(async () => {
    try {
      setMembers(await fetchMembers(ws.id));
      if (canManage) setInvites(await fetchInvites(ws.id));
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load members'));
      setMembers((m) => m ?? []);
    }
  }, [ws.id, canManage]);

  useEffect(() => {
    void load();
  }, [load]);

  async function invite(e: FormEvent) {
    e.preventDefault();
    if (!email.trim()) return;
    setBusy('invite');
    try {
      const r = await createInvite(ws.id, email.trim(), role);
      if (r.link) setLinks((l) => ({ ...l, [r.invite.id]: r.link ?? '' }));
      toast.push(r.delivery === 'manual' ? 'Invitation created. Copy the link below and send it yourself.' : `Invitation sent to ${email.trim()}`, 'success');
      setEmail('');
      await load();
    } catch (err) {
      toast.push(errText(err, 'Could not invite'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function changeRole(m: MemberRow, next: MemberRole) {
    setBusy(m.user_id);
    try {
      await patchMember(ws.id, m.user_id, next);
      toast.push(`${m.email} is now ${next}`, 'success');
      await load();
    } catch (e) {
      toast.push(errText(e, 'Could not change the role'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function remove(m: MemberRow) {
    const self = m.user_id === user?.id;
    if (!window.confirm(self ? `Leave ${ws.name}? You lose access to its sites.` : `Remove ${m.email} from ${ws.name}?`)) return;
    setBusy(m.user_id);
    try {
      await removeMember(ws.id, m.user_id);
      toast.push(self ? `You left ${ws.name}` : `${m.email} removed`, 'success');
      if (self) {
        await reloadWorkspaces();
        navigate('/sites');
      } else await load();
    } catch (e) {
      toast.push(errText(e, 'Could not remove the member'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function revoke(i: InviteRow) {
    if (!window.confirm(`Revoke the invitation for ${i.email}? The link stops working.`)) return;
    setBusy(i.id);
    try {
      await revokeInvite(ws.id, i.id);
      toast.push('Invitation revoked', 'success');
      await load();
    } catch (e) {
      toast.push(errText(e, 'Could not revoke'), 'error');
    } finally {
      setBusy(null);
    }
  }

  const pending = (invites ?? []).filter((i) => !i.accepted_at);

  return (
    <>
      <div className="card">
        <div className="card-title">Members</div>
        {error && <div className="error-box">{error}</div>}
        {members === null ? (
          <Skeleton rows={3} />
        ) : (
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>Member</th>
                  <th>Role</th>
                  <th>Joined</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {members.map((m) => {
                  const self = m.user_id === user?.id;
                  // The owner changes only via transfer; only the owner may change an admin.
                  const editable = canManage && m.role !== 'owner' && !self && (m.role !== 'admin' || isOwnerish);
                  return (
                    <tr key={m.user_id}>
                      <td>
                        <div>{m.name || m.email}</div>
                        {m.name && <div className="faint field-note">{m.email}</div>}
                      </td>
                      <td>
                        {editable ? (
                          <select
                            aria-label={`Role of ${m.email}`}
                            value={m.role}
                            disabled={busy === m.user_id}
                            onChange={(e) => void changeRole(m, e.target.value as MemberRole)}
                          >
                            {MEMBER_ROLES.filter((r) => r.id !== 'admin' || isOwnerish || m.role === 'admin').map((r) => (
                              <option key={r.id} value={r.id}>
                                {r.label}
                              </option>
                            ))}
                          </select>
                        ) : (
                          <span className={`role-chip role-ws-${m.role}`}>{m.role}</span>
                        )}
                        {self && <span className="faint"> (you)</span>}
                      </td>
                      <td className="muted">{fmtDate(m.created_at)}</td>
                      <td>
                        {m.role !== 'owner' && (editable || self) && (
                          <button className="btn btn-sm btn-danger" disabled={busy === m.user_id} onClick={() => void remove(m)}>
                            {self ? 'Leave' : 'Remove'}
                          </button>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        <div className="role-legend">
          {MEMBER_ROLES.map((r) => (
            <span key={r.id}>
              <b>{r.label}</b> {r.hint}
            </span>
          ))}
          <span>
            <b>Owner</b> Also handles the plan, ownership and deletion.
          </span>
        </div>
      </div>

      {canManage && (
        <div className="card">
          <div className="card-title">Invite someone</div>
          <form className="invite-form" onSubmit={(e) => void invite(e)}>
            <label className="field">
              Email
              <input type="email" value={email} onChange={(e) => setEmail(e.target.value)} placeholder="colleague@example.com" />
            </label>
            <label className="field">
              Role
              <select value={role} onChange={(e) => setRole(e.target.value as MemberRole)}>
                {MEMBER_ROLES.filter((r) => r.id !== 'admin' || isOwnerish || myRole === 'admin').map((r) => (
                  <option key={r.id} value={r.id}>
                    {r.label}
                  </option>
                ))}
              </select>
            </label>
            <button className="btn btn-primary" disabled={busy === 'invite' || !email.trim()}>
              {busy === 'invite' ? 'Inviting…' : 'Send invitation'}
            </button>
          </form>
          <div className="faint field-note">Invitations expire after 7 days. Pending invitations count toward the member limit.</div>

          <div className="card-title" style={{ marginTop: 18 }}>
            Pending invitations
          </div>
          {invites === null ? (
            <Skeleton rows={2} />
          ) : pending.length === 0 ? (
            <div className="muted">No pending invitations.</div>
          ) : (
            <div className="tbl-wrap">
              <table className="tbl">
                <thead>
                  <tr>
                    <th>Email</th>
                    <th>Role</th>
                    <th>Expires</th>
                    <th>Link</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {pending.map((i) => {
                    const link = links[i.id] ?? i.link ?? null;
                    const expired = new Date(i.expires_at).getTime() < Date.now();
                    return (
                      <tr key={i.id}>
                        <td>{i.email}</td>
                        <td>{i.role}</td>
                        <td className={expired ? 'tone-bad' : 'muted'}>{expired ? 'expired' : fmtDate(i.expires_at)}</td>
                        <td>{link ? <CopyButton text={absoluteLink(link)} label="Copy link" what="Invitation link" /> : <span className="faint">emailed</span>}</td>
                        <td>
                          <button className="btn btn-sm btn-danger" disabled={busy === i.id} onClick={() => void revoke(i)}>
                            Revoke
                          </button>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------

function PlanTab({ ws }: { ws: WorkspaceDetail }) {
  const [report, setReport] = useState<UsageReport | null>(null);
  const [billing, setBilling] = useState<BillingInfo | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setReport(null);
    fetchUsageReport(ws.id, true)
      .then((r) => alive && setReport(r))
      .catch((e: unknown) => alive && setError(errText(e, 'Could not load usage')));
    fetchBilling(ws.id)
      .then((b) => alive && setBilling(b))
      .catch(() => {
        /* billing info is optional */
      });
    return () => {
      alive = false;
    };
  }, [ws.id]);

  return (
    <>
      <div className="card plan-card">
        <div className="plan-head">
          <div>
            <div className="plan-name">{report?.plan.name ?? ws.plan?.name ?? 'Plan'}</div>
            {report?.plan.price_label && <div className="muted">{report.plan.price_label}</div>}
          </div>
          <div className="spacer" />
          {report && <div className="faint">Usage for {report.month} (UTC). Monthly counters reset on the 1st.</div>}
        </div>
        {error && <div className="error-box">{error}</div>}
        {!report && !error ? <Skeleton rows={5} /> : report && <UsageBars limits={report.limits} usage={report.usage} />}
      </div>

      <div className="card">
        <div className="card-title">Upgrades</div>
        {billing?.provider === 'manual' || billing?.provider === 'none' || !billing ? (
          <p className="muted card-lead">
            Plans are assigned by the platform admin.{' '}
            {billing?.upgrade?.contact ? (
              <>
                To change plan, contact{' '}
                {billing.upgrade.contact.includes('@') ? <a href={`mailto:${billing.upgrade.contact}`}>{billing.upgrade.contact}</a> : billing.upgrade.contact}.
              </>
            ) : (
              'Ask them when you need higher limits.'
            )}
          </p>
        ) : (
          <p className="muted card-lead">Billing provider: {billing.provider}.</p>
        )}
        {billing?.upgrade?.note && <p className="muted">{billing.upgrade.note}</p>}
      </div>

      {report?.history && report.history.length > 0 && (
        <div className="card">
          <div className="card-title">History</div>
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>Month</th>
                  <th className="num">Sites</th>
                  <th className="num">Posts</th>
                  <th className="num">Link runs</th>
                  <th className="num">AI tokens</th>
                  <th className="num">Members</th>
                </tr>
              </thead>
              <tbody>
                {report.history.map((h) => (
                  <tr key={h.month}>
                    <td>{h.month}</td>
                    <td className="num">{h.sites.toLocaleString()}</td>
                    <td className="num">{h.posts.toLocaleString()}</td>
                    <td className="num">{h.link_runs.toLocaleString()}</td>
                    <td className="num">{h.ai_tokens.toLocaleString()}</td>
                    <td className="num">{h.members.toLocaleString()}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------

function DangerTab({ ws, canDanger }: { ws: WorkspaceDetail; canDanger: boolean }) {
  const toast = useToast();
  const navigate = useNavigate();
  const { reload, switchTo, mine } = useWorkspace();
  const { user } = useUser();
  const [members, setMembers] = useState<MemberRow[]>([]);
  const [target, setTarget] = useState('');
  const [confirm, setConfirm] = useState('');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    fetchMembers(ws.id)
      .then((m) => setMembers(m.filter((x) => x.role !== 'owner')))
      .catch(() => setMembers([]));
  }, [ws.id]);

  if (!canDanger) {
    return (
      <div className="card">
        <div className="muted">Only the workspace owner can transfer or delete it.</div>
      </div>
    );
  }

  async function transfer() {
    const m = members.find((x) => x.user_id === target);
    if (!m || !window.confirm(`Make ${m.email} the owner of ${ws.name}? You stay on as an admin.`)) return;
    setBusy(true);
    try {
      await transferWorkspace(ws.id, target);
      toast.push(`${m.email} now owns ${ws.name}`, 'success');
      await reload();
    } catch (e) {
      toast.push(errText(e, 'Could not transfer'), 'error');
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    setBusy(true);
    try {
      await deleteWorkspace(ws.id, confirm.trim());
      toast.push(`${ws.name} deleted. It can be restored by the platform admin for 30 days.`, 'success');
      const next = mine.find((w) => w.id !== ws.id);
      if (next) switchTo(next.id);
      await reload();
      navigate('/sites');
    } catch (e) {
      toast.push(errText(e, 'Could not delete'), 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="card danger-card">
      <div className="danger-row">
        <div>
          <div className="settings-row-label">Transfer ownership</div>
          <div className="settings-row-desc">The new owner must already be a member. You become an admin.</div>
        </div>
        <div className="danger-actions">
          <select aria-label="New owner" value={target} onChange={(e) => setTarget(e.target.value)}>
            <option value="">Choose a member…</option>
            {members
              .filter((m) => m.user_id !== user?.id)
              .map((m) => (
                <option key={m.user_id} value={m.user_id}>
                  {m.name ? `${m.name} (${m.email})` : m.email}
                </option>
              ))}
          </select>
          <button className="btn" disabled={busy || !target} onClick={() => void transfer()}>
            Transfer
          </button>
        </div>
      </div>
      <div className="danger-row">
        <div>
          <div className="settings-row-label">Delete workspace</div>
          <div className="settings-row-desc">
            {ws.is_default
              ? 'The Default workspace cannot be deleted.'
              : 'Sites, runs and tokens of this workspace stop working at once. The platform admin can restore it for 30 days; after that it is purged.'}
          </div>
        </div>
        {!ws.is_default && (
          <div className="danger-actions">
            <input
              className="input mono"
              aria-label={`Type ${ws.slug} to confirm`}
              placeholder={`Type ${ws.slug}`}
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
            />
            <button className="btn btn-danger" disabled={busy || confirm.trim() !== ws.slug} onClick={() => void remove()}>
              Delete workspace
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------

export function WorkspacePage() {
  const [params, setParams] = useSearchParams();
  const { user } = useUser();
  const wsCtx = useWorkspace();
  const [detail, setDetail] = useState<WorkspaceDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const id = wsCtx.viewAs?.id ?? wsCtx.current?.id ?? null;

  const raw = params.get('tab');
  const tab: TabId = TABS.some((t) => t.id === raw) ? (raw as TabId) : 'profile';

  const load = useCallback(async () => {
    if (!id) return;
    try {
      setDetail(await fetchWorkspace(id));
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load the workspace'));
    }
  }, [id]);

  useEffect(() => {
    setDetail(null);
    void load();
  }, [load, wsCtx.scopeKey]);

  const head = (
    <div className="page-head">
      <div>
        <h1>{detail?.name ?? 'Workspace'}</h1>
        <div className="page-sub">{TABS.find((t) => t.id === tab)?.hint}</div>
      </div>
      <div className="spacer" />
      <HelpButton />
    </div>
  );

  if (!wsCtx.available) {
    return (
      <div>
        {head}
        <div className="card">
          <div className="empty">
            <div className="empty-title">Workspaces need SiloForge 2.4</div>
            <div className="empty-hint">This server does not have workspaces yet. Everything works as a single workspace.</div>
          </div>
        </div>
      </div>
    );
  }

  if (!id) {
    return (
      <div>
        {head}
        <div className="card">
          <div className="empty">
            <div className="empty-title">{wsCtx.loading ? 'Loading workspaces…' : 'Choose a workspace'}</div>
            {!wsCtx.loading && (
              <div className="empty-hint">
                You are viewing all workspaces. Pick one to see its settings:
                <div className="ws-pick-list">
                  {wsCtx.mine.map((w) => (
                    <button key={w.id} className="btn btn-sm" onClick={() => wsCtx.switchTo(w.id)}>
                      {w.name}
                    </button>
                  ))}
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    );
  }

  const platform = isPlatformAdmin(user);
  const readOnly = !!wsCtx.viewAs;
  const myRole = readOnly ? 'viewer' : (detail?.my_role ?? wsCtx.role ?? 'viewer');
  const canManage = !readOnly && (platform || roleAtLeast(myRole, 'admin'));
  const canDanger = !readOnly && (platform || myRole === 'owner');

  return (
    <div>
      {head}
      <div className="tabs" role="tablist">
        {TABS.map((t) => (
          <button
            key={t.id}
            role="tab"
            aria-selected={tab === t.id}
            className={`tab${tab === t.id ? ' active' : ''}`}
            data-hint={t.hint}
            onClick={() => setParams(t.id === 'profile' ? {} : { tab: t.id })}
          >
            {t.label}
          </button>
        ))}
        <div className="spacer" />
        <span className={`role-chip role-ws-${myRole}`} title="Your role in this workspace">
          {myRole === 'platform_admin' ? 'platform admin' : myRole}
        </span>
      </div>
      {error && <div className="error-box">{error}</div>}
      {!detail && !error ? (
        <Skeleton rows={6} />
      ) : (
        detail && (
          <div className="tab-body">
            {tab === 'profile' && (
              <ProfileTab
                ws={detail}
                canManage={canManage}
                onSaved={() => {
                  void load();
                  void wsCtx.reload();
                }}
              />
            )}
            {tab === 'members' && <MembersTab ws={detail} myRole={myRole} canManage={canManage} />}
            {tab === 'plan' && <PlanTab ws={detail} />}
            {tab === 'tokens' && <ApiMcpPanel key={detail.id} workspaceId={detail.id} canManage={canManage} />}
            {tab === 'danger' && <DangerTab ws={detail} canDanger={canDanger} />}
          </div>
        )
      )}
    </div>
  );
}
