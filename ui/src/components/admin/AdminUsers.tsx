import { useCallback, useEffect, useState } from 'react';
import { fmtDate } from '../../format';
import {
  adminUserAction,
  createAdminUser,
  deleteAdminUser,
  fetchAdminUsers,
  fetchAdminWorkspaces,
  patchAdminUser,
  type AdminUser,
  type AdminUserAction,
  type AdminWorkspace,
  type MemberRole,
} from '../../saas';
import { useToast } from '../../toast';
import { useUser } from '../../user';
import { Modal } from '../Modal';
import { Skeleton } from '../Skeleton';
import { errText, LinkModal, Pager, useDebounced } from './common';

const LIMIT = 50;

function CreateUserModal({ onClose, onDone }: { onClose: () => void; onDone: (link?: string, email?: string) => void }) {
  const [email, setEmail] = useState('');
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [role, setRole] = useState<'user' | 'admin'>('user');
  const [workspaces, setWorkspaces] = useState<AdminWorkspace[]>([]);
  const [workspaceId, setWorkspaceId] = useState('');
  const [workspaceRole, setWorkspaceRole] = useState<MemberRole>('member');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    fetchAdminWorkspaces({ status: 'active', limit: 200 })
      .then((r) => setWorkspaces(r.workspaces))
      .catch(() => setWorkspaces([]));
  }, []);

  async function submit() {
    setError(null);
    if (!email.trim()) return setError('Enter an email address.');
    if (password && password.length < 8) return setError('Use at least 8 characters, or leave the password empty to send a set-password link.');
    setBusy(true);
    try {
      const r = await createAdminUser({
        email: email.trim(),
        name: name.trim() || undefined,
        password: password || undefined,
        role,
        workspace_id: workspaceId || undefined,
        workspace_role: workspaceId ? workspaceRole : undefined,
      });
      onDone(r.link, email.trim());
    } catch (e) {
      setError(errText(e, 'Could not create the user'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      title="Create user"
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={busy} onClick={() => void submit()}>
            {busy ? 'Creating…' : 'Create user'}
          </button>
        </>
      }
    >
      <div className="form-grid" style={{ maxWidth: 'none' }}>
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          Email
          <input type="email" value={email} onChange={(e) => setEmail(e.target.value)} autoFocus />
        </label>
        <label className="field">
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Optional" />
        </label>
        <label className="field">
          Password
          <input type="password" autoComplete="new-password" value={password} onChange={(e) => setPassword(e.target.value)} />
          <span className="field-note faint">Leave empty to send a link where they choose their own.</span>
        </label>
        <label className="field">
          Platform role
          <select value={role} onChange={(e) => setRole(e.target.value as 'user' | 'admin')}>
            <option value="user">User</option>
            <option value="admin">Platform admin (full access to everything)</option>
          </select>
        </label>
        <label className="field">
          Add to workspace
          <select value={workspaceId} onChange={(e) => setWorkspaceId(e.target.value)}>
            <option value="">None</option>
            {workspaces.map((w) => (
              <option key={w.id} value={w.id}>
                {w.name}
              </option>
            ))}
          </select>
        </label>
        {workspaceId && (
          <label className="field">
            Workspace role
            <select value={workspaceRole} onChange={(e) => setWorkspaceRole(e.target.value as MemberRole)}>
              <option value="viewer">Viewer</option>
              <option value="member">Member</option>
              <option value="admin">Admin</option>
            </select>
          </label>
        )}
      </div>
    </Modal>
  );
}

function EditUserModal({ u, onClose, onSaved }: { u: AdminUser; onClose: () => void; onSaved: () => void }) {
  const toast = useToast();
  const { user } = useUser();
  const [name, setName] = useState(u.name);
  const [email, setEmail] = useState(u.email);
  const [role, setRole] = useState(u.role);
  const [busy, setBusy] = useState(false);
  const self = u.id === user?.id;

  async function save() {
    setBusy(true);
    try {
      await patchAdminUser(u.id, {
        ...(name !== u.name ? { name: name.trim() } : {}),
        ...(email !== u.email ? { email: email.trim() } : {}),
        ...(role !== u.role ? { role } : {}),
      });
      toast.push('User saved', 'success');
      onSaved();
    } catch (e) {
      toast.push(errText(e, 'Could not save'), 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      title={`Edit ${u.email}`}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={busy} onClick={() => void save()}>
            {busy ? 'Saving…' : 'Save'}
          </button>
        </>
      }
    >
      <div className="form-grid" style={{ maxWidth: 'none' }}>
        <label className="field">
          Name
          <input value={name} onChange={(e) => setName(e.target.value)} />
        </label>
        <label className="field">
          Email
          <input type="email" value={email} onChange={(e) => setEmail(e.target.value)} />
          <span className="field-note faint">Changed here, the new address counts as verified.</span>
        </label>
        <label className="field">
          Platform role
          <select value={role} disabled={self} onChange={(e) => setRole(e.target.value as 'user' | 'admin')}>
            <option value="user">User</option>
            <option value="admin">Platform admin</option>
          </select>
          {self && <span className="field-note faint">You cannot change your own platform role.</span>}
        </label>
      </div>
    </Modal>
  );
}

function DeleteUserModal({ u, onClose, onDone }: { u: AdminUser; onClose: () => void; onDone: () => void }) {
  const { user } = useUser();
  const toast = useToast();
  const [targets, setTargets] = useState<AdminUser[]>([]);
  const [to, setTo] = useState('');
  const [busy, setBusy] = useState(false);
  const owns = u.workspaces.filter((w) => w.role === 'owner');

  useEffect(() => {
    fetchAdminUsers({ limit: 200 })
      .then((r) => setTargets(r.users.filter((x) => x.id !== u.id && !x.disabled_at)))
      .catch(() => setTargets([]));
  }, [u.id]);

  async function remove() {
    setBusy(true);
    try {
      await deleteAdminUser(u.id, to || undefined);
      toast.push(`${u.email} deleted`, 'success');
      onDone();
    } catch (e) {
      toast.push(errText(e, 'Could not delete'), 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      title={`Delete ${u.email}?`}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-danger" disabled={busy} onClick={() => void remove()}>
            {busy ? 'Deleting…' : 'Delete user'}
          </button>
        </>
      }
    >
      <p className="muted" style={{ marginTop: 0 }}>
        The account and its sessions and tokens are removed. This cannot be undone.
        {owns.length > 0 && ` They own ${owns.map((w) => w.name).join(', ')}; ownership and their sites move to the person below.`}
      </p>
      <label className="field">
        Transfer workspaces and sites to
        <select value={to} onChange={(e) => setTo(e.target.value)}>
          <option value="">Me ({user?.email})</option>
          {targets
            .filter((t) => t.id !== user?.id)
            .map((t) => (
              <option key={t.id} value={t.id}>
                {t.name ? `${t.name} (${t.email})` : t.email}
              </option>
            ))}
        </select>
      </label>
    </Modal>
  );
}

const ACTION_LABEL: Record<AdminUserAction, string> = {
  'reset-password': 'Password reset link created',
  logout: 'Signed out everywhere',
  disable: 'User disabled',
  enable: 'User enabled',
  'verify-email': 'Email marked as verified',
  'reset-2fa': 'Two-factor sign-in turned off',
};

export function AdminUsers() {
  const toast = useToast();
  const { user: me } = useUser();
  const [input, q, setQ] = useDebounced();
  const [offset, setOffset] = useState(0);
  const [rows, setRows] = useState<AdminUser[] | null>(null);
  const [total, setTotal] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<AdminUser | null>(null);
  const [deleting, setDeleting] = useState<AdminUser | null>(null);
  const [link, setLink] = useState<{ title: string; text: string; link: string } | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const r = await fetchAdminUsers({ q: q || undefined, limit: LIMIT, offset });
      setRows(r.users);
      setTotal(r.total);
      setError(null);
    } catch (e) {
      setError(errText(e, 'Could not load users'));
      setRows([]);
    }
  }, [q, offset]);

  useEffect(() => {
    void load();
  }, [load]);
  useEffect(() => setOffset(0), [q]);

  async function act(u: AdminUser, action: AdminUserAction) {
    if (action === 'disable' && !window.confirm(`Disable ${u.email}? They are signed out and cannot sign in until enabled.`)) return;
    if (action === 'reset-2fa' && !window.confirm(`Turn off two-factor sign-in for ${u.email}? Do this only after confirming who is asking.`)) return;
    setBusy(u.id);
    try {
      const r = await adminUserAction(u.id, action);
      if (r.link) setLink({ title: 'Password reset link', text: `Send this link to ${u.email}. It works once and expires in one hour.`, link: r.link });
      toast.push(action === 'reset-password' && r.delivery === 'smtp' ? `Reset link emailed to ${u.email}` : ACTION_LABEL[action], 'success');
      await load();
    } catch (e) {
      toast.push(errText(e, 'Action failed'), 'error');
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="card">
      <div className="table-toolbar">
        <input className="input" type="search" placeholder="Search by email or name" aria-label="Search users" value={input} onChange={(e) => setQ(e.target.value)} />
        <div className="spacer" />
        <button className="btn btn-primary btn-sm" onClick={() => setCreating(true)}>
          Create user
        </button>
      </div>
      {error && <div className="error-box">{error}</div>}
      {rows === null ? (
        <Skeleton rows={6} />
      ) : rows.length === 0 ? (
        <div className="empty">
          <div className="empty-title">No users match</div>
          <div className="empty-hint">Clear the search, or create a user.</div>
        </div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>User</th>
                <th>Role</th>
                <th>Workspaces</th>
                <th>State</th>
                <th>Last sign-in</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rows.map((u) => {
                const self = u.id === me?.id;
                return (
                  <tr key={u.id} className={u.disabled_at ? 'row-dim' : undefined}>
                    <td>
                      <div>{u.name || u.email}</div>
                      {u.name && <div className="faint field-note">{u.email}</div>}
                    </td>
                    <td>
                      <span className={`role-chip role-${u.role}`}>{u.role === 'admin' ? 'platform admin' : 'user'}</span>
                    </td>
                    <td className="muted">
                      {u.workspaces.length === 0 ? '—' : u.workspaces.map((w) => `${w.name} (${w.role})`).join(', ')}
                    </td>
                    <td>
                      <span className="state-tags">
                        {u.disabled_at && <span className="chip chip-failed">disabled</span>}
                        {!u.email_verified_at && <span className="chip chip-reverted">unverified</span>}
                        {u.two_factor_enabled && <span className="chip chip-done">2FA</span>}
                        {!u.disabled_at && u.email_verified_at && !u.two_factor_enabled && <span className="faint">active</span>}
                      </span>
                      <div className="faint field-note">{u.sessions_active} active session{u.sessions_active === 1 ? '' : 's'}</div>
                    </td>
                    <td className="muted">{u.last_login_at ? fmtDate(u.last_login_at) : 'never'}</td>
                    <td>
                      <div className="row-actions">
                        <button className="btn btn-sm" onClick={() => setEditing(u)}>
                          Edit
                        </button>
                        <div className="menu-wrap">
                          <button className="btn btn-sm" aria-expanded={open === u.id} disabled={busy === u.id} onClick={() => setOpen(open === u.id ? null : u.id)}>
                            More
                          </button>
                          {open === u.id && (
                            <div className="row-menu" role="menu" onClick={() => setOpen(null)}>
                              <button role="menuitem" onClick={() => void act(u, 'reset-password')}>
                                Send password reset link
                              </button>
                              <button role="menuitem" onClick={() => void act(u, 'logout')}>
                                Sign out everywhere
                              </button>
                              {!u.email_verified_at && (
                                <button role="menuitem" onClick={() => void act(u, 'verify-email')}>
                                  Mark email verified
                                </button>
                              )}
                              {u.two_factor_enabled && (
                                <button role="menuitem" onClick={() => void act(u, 'reset-2fa')}>
                                  Reset two-factor sign-in
                                </button>
                              )}
                              {!self &&
                                (u.disabled_at ? (
                                  <button role="menuitem" onClick={() => void act(u, 'enable')}>
                                    Enable account
                                  </button>
                                ) : (
                                  <button role="menuitem" onClick={() => void act(u, 'disable')}>
                                    Disable account
                                  </button>
                                ))}
                              {!self && (
                                <button role="menuitem" className="danger" onClick={() => setDeleting(u)}>
                                  Delete user…
                                </button>
                              )}
                            </div>
                          )}
                        </div>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      <Pager offset={offset} limit={LIMIT} total={total} onPage={setOffset} />
      {creating && (
        <CreateUserModal
          onClose={() => setCreating(false)}
          onDone={(l, email) => {
            setCreating(false);
            toast.push('User created', 'success');
            if (l) setLink({ title: 'Set-password link', text: `Send this link to ${email}. They choose their password there.`, link: l });
            void load();
          }}
        />
      )}
      {editing && (
        <EditUserModal
          u={editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void load();
          }}
        />
      )}
      {deleting && (
        <DeleteUserModal
          u={deleting}
          onClose={() => setDeleting(null)}
          onDone={() => {
            setDeleting(null);
            void load();
          }}
        />
      )}
      {link && <LinkModal title={link.title} text={link.text} link={link.link} onClose={() => setLink(null)} />}
    </div>
  );
}
