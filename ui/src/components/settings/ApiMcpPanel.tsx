// Settings → API & MCP (merged): API tokens with editable scopes/expiry and per-token tool
// allowlists, the MCP tool manager (enable/disable, minimum scope, 24 h stats, grouped with
// search and bulk actions), the HTTP transport switch, and connection info for Hermes.

import { Fragment, useCallback, useEffect, useMemo, useState } from 'react';
import { ApiError } from '../../api';
import {
  fetchMcpInfo,
  fetchMcpPolicy,
  fetchMcpStats,
  fetchTokens,
  isUnavailable,
  patchToken,
  putMcpPolicy,
  revokeToken,
  v2ErrorMessage,
} from '../../api2';
import { fetchWorkspaceMcpPolicy, putWorkspaceMcpPolicy } from '../../saas';
import { fmtDate } from '../../format';
import { useToast } from '../../toast';
import { useUser } from '../../user';
import type { ApiTokenCreated, ApiTokenRow, McpInfo, McpPolicy, McpToolInfo, McpToolStat, MinScope, TokenScope } from '../../types2';
import { CreateTokenModal, EXPIRY, SCOPES, ShowOnceModal, tokenState } from '../ApiTokensPanel';
import { McpPanel } from '../McpPanel';
import { Modal } from '../Modal';
import { Skeleton } from '../Skeleton';
import { Toggle } from '../Toggle';
import { FeatureUnavailable } from '../V2Notice';

/** Admin-only generic API passthrough: disabled unless an admin turns it on. */
const ESCAPE_HATCH = 'siloforge_api_request';
const MIN_SCOPES: MinScope[] = ['read', 'write', 'admin'];

/** Category rules for tool names; first match wins. The server's `category`/`group` wins over these. */
const CATEGORY_RULES: Array<[string, RegExp]> = [
  ['Escape hatch', /api_request/],
  ['Orientation', /capabilit|help|whoami/],
  ['MCP policy', /mcp|policy/],
  ['API tokens', /token/],
  ['Logs', /(^|_)logs?(_|$)/],
  ['Docs', /(^|_)docs?(_|$)/],
  ['Downloads', /download/],
  ['Backups', /backup|restore/],
  ['Notifications', /notif|channel/],
  ['AI models', /model|provider|chain|rung|probe|_ai_|llm/],
  ['Settings', /settings/],
  ['Schedules', /schedule/],
  ['Jobs', /job/],
  ['Rules', /rule|ignore|keyword/],
  ['Silo and clusters', /silo|cluster/],
  ['Inject and undo', /inject|undo|apply|revert/],
  ['Edges and anchors', /edge|anchor|decide|approve|reject|score/],
  ['Reports', /report|orphan|cannibal|metric|export|graph|history/],
  ['Crawl, plan and audits', /crawl|plan|audit|link_health|gsc|health_check/],
  ['Posts', /post/],
  ['Sites', /site/],
  ['System', /system|doctor|restart|status|health|version/],
];

function categoryOf(t: McpToolInfo): string {
  const explicit = t.category ?? t.group;
  if (explicit) return explicit.charAt(0).toUpperCase() + explicit.slice(1);
  const n = t.name.replace(/^siloforge_/, '');
  for (const [label, re] of CATEGORY_RULES) if (re.test(n)) return label;
  return 'Other';
}

function defaultScope(t: McpToolInfo): MinScope {
  const m = t.min_scope;
  if (m === 'read' || m === 'write' || m === 'admin') return m;
  return t.read_only ? 'read' : 'write';
}

function emptyPolicy(info: McpInfo | null): McpPolicy {
  return { http_enabled: info?.http_enabled ?? true, tools: {}, per_token: {} };
}

function normPolicy(p: Partial<McpPolicy> | null | undefined, info: McpInfo | null): McpPolicy {
  return { ...emptyPolicy(info), ...(p ?? {}), tools: { ...(p?.tools ?? {}) }, per_token: { ...(p?.per_token ?? {}) } };
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

function TokenEditRow({
  t,
  isAdmin,
  onCancel,
  onSaved,
}: {
  t: ApiTokenRow;
  isAdmin: boolean;
  onCancel: () => void;
  onSaved: (row: ApiTokenRow) => void;
}) {
  const toast = useToast();
  const [name, setName] = useState(t.name);
  const [scopes, setScopes] = useState<TokenScope[]>(t.scopes.filter((s): s is TokenScope => ['*', 'read', 'write', 'admin'].includes(s)));
  const [expiry, setExpiry] = useState<string>('keep');
  const [busy, setBusy] = useState(false);

  async function save() {
    if (scopes.length === 0) {
      toast.push('Pick at least one scope', 'error');
      return;
    }
    const body: { name?: string; scopes?: TokenScope[]; expires_in_days?: number | null } = {};
    if (name.trim() && name.trim() !== t.name) body.name = name.trim();
    if ([...scopes].sort().join() !== [...t.scopes].sort().join()) body.scopes = scopes;
    if (expiry !== 'keep') body.expires_in_days = expiry === 'never' ? null : Number(expiry);
    if (Object.keys(body).length === 0) {
      onCancel();
      return;
    }
    setBusy(true);
    try {
      const row = await patchToken(t.id, body);
      toast.push('Token updated. New scopes apply immediately.', 'success');
      onSaved(row);
    } catch (e) {
      const msg =
        e instanceof ApiError && e.status === 404 && !isUnavailable(e)
          ? 'This server cannot edit tokens yet (needs SiloForge 2.1).'
          : v2ErrorMessage(e, 'Could not update the token');
      toast.push(msg, 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <tr className="edit-row">
      <td colSpan={8}>
        <div className="token-edit">
          <label className="field">
            Name
            <input className="input" value={name} onChange={(e) => setName(e.target.value)} />
          </label>
          <div className="field">
            Scopes
            <div className="scope-pills">
              {SCOPES.filter((s) => !s.adminOnly || isAdmin || t.scopes.includes(s.id)).map((s) => {
                const on = scopes.includes(s.id);
                const locked = s.adminOnly && !isAdmin;
                return (
                  <button
                    key={s.id}
                    type="button"
                    className={`scope-pill${on ? ' on' : ''}`}
                    aria-pressed={on}
                    disabled={locked}
                    title={s.hint}
                    onClick={() => setScopes(on ? scopes.filter((x) => x !== s.id) : [...scopes, s.id])}
                  >
                    {s.label}
                  </button>
                );
              })}
            </div>
            <span className="faint field-note">A token can never have more access than your account.</span>
          </div>
          <label className="field">
            Expiry
            <select value={expiry} onChange={(e) => setExpiry(e.target.value)}>
              <option value="keep">Keep ({t.expires_at ? fmtDate(t.expires_at) : 'never'})</option>
              {EXPIRY.map((x) => (
                <option key={x.label} value={x.days === null ? 'never' : String(x.days)}>
                  {x.days === null ? 'Never expires' : `${x.label} from now`}
                </option>
              ))}
            </select>
          </label>
          <div className="form-actions">
            <button className="btn btn-sm" onClick={onCancel} disabled={busy}>
              Cancel
            </button>
            <button className="btn btn-sm btn-primary" onClick={() => void save()} disabled={busy}>
              {busy ? 'Saving…' : 'Save token'}
            </button>
          </div>
        </div>
      </td>
    </tr>
  );
}

function AllowlistModal({
  token,
  tools,
  current,
  onClose,
  onSave,
}: {
  token: ApiTokenRow;
  tools: McpToolInfo[];
  current: string[] | null;
  onClose: () => void;
  onSave: (allow: string[] | null) => Promise<boolean>;
}) {
  const [mode, setMode] = useState<'all' | 'list'>(current ? 'list' : 'all');
  const [sel, setSel] = useState<Set<string>>(new Set(current ?? []));
  const [q, setQ] = useState('');
  const [busy, setBusy] = useState(false);
  const groups = useMemo(() => {
    const needle = q.trim().toLowerCase();
    const m = new Map<string, McpToolInfo[]>();
    for (const t of tools) {
      if (needle && !`${t.name} ${t.title ?? ''}`.toLowerCase().includes(needle)) continue;
      const c = categoryOf(t);
      m.set(c, [...(m.get(c) ?? []), t]);
    }
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  }, [tools, q]);

  return (
    <Modal
      title={`Tools for ${token.name}`}
      wide
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn btn-primary"
            disabled={busy || (mode === 'list' && sel.size === 0)}
            onClick={() => {
              setBusy(true);
              void onSave(mode === 'all' ? null : [...sel].sort()).then((ok) => {
                setBusy(false);
                if (ok) onClose();
              });
            }}
          >
            {busy ? 'Saving…' : 'Save tool access'}
          </button>
        </>
      }
    >
      <div className="radio-stack">
        <label className="check-line">
          <input type="radio" checked={mode === 'all'} onChange={() => setMode('all')} />
          <span>
            <b>Every enabled tool its scopes allow</b> <span className="faint">(default)</span>
          </span>
        </label>
        <label className="check-line">
          <input type="radio" checked={mode === 'list'} onChange={() => setMode('list')} />
          <span>
            <b>Only the tools ticked below</b> <span className="faint">({sel.size} selected)</span>
          </span>
        </label>
      </div>
      {mode === 'list' && (
        <>
          <input className="input" type="search" placeholder="Filter tools" value={q} onChange={(e) => setQ(e.target.value)} aria-label="Filter tools" />
          <div className="allow-groups">
            {groups.map(([cat, list]) => {
              const all = list.every((t) => sel.has(t.name));
              return (
                <div key={cat} className="allow-group">
                  <label className="check-line allow-group-head">
                    <input
                      type="checkbox"
                      checked={all}
                      onChange={() =>
                        setSel((cur) => {
                          const n = new Set(cur);
                          list.forEach((t) => (all ? n.delete(t.name) : n.add(t.name)));
                          return n;
                        })
                      }
                    />
                    <b>{cat}</b>
                  </label>
                  {list.map((t) => (
                    <label key={t.name} className="check-line allow-item">
                      <input
                        type="checkbox"
                        checked={sel.has(t.name)}
                        onChange={() =>
                          setSel((cur) => {
                            const n = new Set(cur);
                            if (n.has(t.name)) n.delete(t.name);
                            else n.add(t.name);
                            return n;
                          })
                        }
                      />
                      <span className="mono-sm">{t.name}</span>
                    </label>
                  ))}
                </div>
              );
            })}
          </div>
        </>
      )}
    </Modal>
  );
}

function TokensCard({
  isAdmin,
  canPolicy,
  canCreate,
  workspaceId,
  tools,
  policy,
  savePolicy,
}: {
  isAdmin: boolean;
  /** May edit per-token tool allowlists (MCP policy). */
  canPolicy: boolean;
  canCreate: boolean;
  /** v2.4: list and create the tokens of this workspace only. */
  workspaceId?: string;
  tools: McpToolInfo[];
  policy: McpPolicy | null;
  savePolicy: (p: McpPolicy, msg: string) => Promise<boolean>;
}) {
  const toast = useToast();
  const [rows, setRows] = useState<ApiTokenRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [creating, setCreating] = useState(false);
  const [fresh, setFresh] = useState<ApiTokenCreated | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [allowFor, setAllowFor] = useState<ApiTokenRow | null>(null);
  const [showRevoked, setShowRevoked] = useState(false);

  const load = useCallback(async () => {
    try {
      setRows(await fetchTokens(workspaceId));
      setError(null);
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(v2ErrorMessage(e, 'Failed to load tokens'));
      setRows([]);
    }
  }, [workspaceId]);

  useEffect(() => {
    void load();
  }, [load]);

  async function revoke(t: ApiTokenRow) {
    if (!window.confirm(`Revoke "${t.name}"? Anything using it stops working immediately.`)) return;
    setBusy(t.id);
    try {
      await revokeToken(t.id);
      toast.push('Token revoked', 'success');
      await load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Could not revoke token'), 'error');
    } finally {
      setBusy(null);
    }
  }

  if (unavailable) {
    return (
      <div className="card">
        <FeatureUnavailable feature="API tokens" />
      </div>
    );
  }

  const visible = (rows ?? []).filter((t) => showRevoked || !t.revoked_at);
  const names = new Map((rows ?? []).map((t) => [t.id, t.name]));

  return (
    <div className="card">
      <div className="table-toolbar">
        <div className="card-title" style={{ marginBottom: 0 }}>
          API tokens
        </div>
        <div className="spacer" />
        <label className="check-line muted">
          <input type="checkbox" checked={showRevoked} onChange={(e) => setShowRevoked(e.target.checked)} /> Show revoked
        </label>
        {canCreate && (
          <button className="btn btn-primary btn-sm" onClick={() => setCreating(true)}>
            Create token
          </button>
        )}
      </div>
      <p className="muted card-lead">
        Tokens act as your account, limited by their scopes: <b>read</b> views data, <b>write</b> changes it (crawl, plan, approve,
        inject), <b>admin</b> manages the server. Edit scopes at any time; changes apply on the next request.
      </p>
      {error && <div className="error-box">{error}</div>}
      {rows === null ? (
        <Skeleton rows={3} />
      ) : visible.length === 0 ? (
        <div className="empty">
          <div className="empty-title">No tokens yet</div>
          <div className="empty-hint">Create one for Hermes Agent, the MCP server or your own scripts.</div>
        </div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Name</th>
                <th>Token</th>
                <th>Scopes</th>
                <th>Tools</th>
                <th>Last used</th>
                <th>Expires</th>
                <th>State</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {visible.map((t) => {
                const st = tokenState(t);
                const allow = policy?.per_token[t.id]?.allow ?? t.tool_policy?.allow ?? null;
                return (
                  <Fragment key={t.id}>
                    <tr>
                      <td>
                        {t.name}
                        {t.parent_id && <div className="faint field-note">from {names.get(t.parent_id) ?? t.parent_id.slice(0, 8)}</div>}
                      </td>
                      <td className="mono-sm">{t.prefix}…</td>
                      <td>
                        <span className="scope-list">
                          {t.scopes.map((s) => (
                            <span key={s} className={`scope-tag scope-${s === '*' ? 'all' : s}`}>
                              {s}
                            </span>
                          ))}
                        </span>
                      </td>
                      <td className="muted">{allow ? `${allow.length} only` : 'all'}</td>
                      <td className="muted">{t.last_used_at ? fmtDate(t.last_used_at) : 'never'}</td>
                      <td className="muted">{t.expires_at ? fmtDate(t.expires_at) : 'never'}</td>
                      <td>
                        <span className={`chip chip-${st.cls}`}>{st.label}</span>
                      </td>
                      <td>
                        {!t.revoked_at && (
                          <div className="row-actions">
                            <button className="btn btn-sm" onClick={() => setEditing(editing === t.id ? null : t.id)}>
                              Edit
                            </button>
                            {canPolicy && policy && (
                              <button className="btn btn-sm" onClick={() => setAllowFor(t)}>
                                Tools
                              </button>
                            )}
                            <button className="btn btn-sm btn-danger" disabled={busy === t.id} onClick={() => void revoke(t)}>
                              Revoke
                            </button>
                          </div>
                        )}
                      </td>
                    </tr>
                    {editing === t.id && (
                      <TokenEditRow
                        t={t}
                        isAdmin={isAdmin}
                        onCancel={() => setEditing(null)}
                        onSaved={() => {
                          setEditing(null);
                          void load();
                        }}
                      />
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {creating && (
        <CreateTokenModal
          workspaceId={workspaceId}
          onClose={() => setCreating(false)}
          onCreated={(t) => {
            setCreating(false);
            setFresh(t);
            void load();
          }}
        />
      )}
      {fresh && <ShowOnceModal token={fresh} onClose={() => setFresh(null)} />}
      {allowFor && policy && (
        <AllowlistModal
          token={allowFor}
          tools={tools}
          current={policy.per_token[allowFor.id]?.allow ?? allowFor.tool_policy?.allow ?? null}
          onClose={() => setAllowFor(null)}
          onSave={(allow) => savePolicy({ ...policy, per_token: { ...policy.per_token, [allowFor.id]: { allow } } }, 'Tool access saved')}
        />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// MCP tool manager
// ---------------------------------------------------------------------------

type ToolFilter = 'all' | 'enabled' | 'disabled' | 'destructive' | 'read_only' | 'errors' | 'used';

function ToolManager({
  info,
  saved,
  stats,
  statsAvailable,
  savePolicy,
  showHttp = true,
}: {
  showHttp?: boolean;
  info: McpInfo;
  saved: McpPolicy;
  stats: Map<string, McpToolStat>;
  statsAvailable: boolean;
  savePolicy: (p: McpPolicy, msg: string) => Promise<boolean>;
}) {
  const [draft, setDraft] = useState<McpPolicy>(saved);
  const [q, setQ] = useState('');
  const [filter, setFilter] = useState<ToolFilter>('all');
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  useEffect(() => setDraft(saved), [saved]);

  const tools = info.tools ?? [];
  const enabledOf = useCallback(
    (t: McpToolInfo) => draft.tools[t.name]?.enabled ?? t.enabled ?? t.name !== ESCAPE_HATCH,
    [draft],
  );
  const scopeOf = useCallback((t: McpToolInfo) => (draft.tools[t.name]?.min_scope as MinScope | undefined) ?? defaultScope(t), [draft]);

  const setTool = (t: McpToolInfo, patch: Partial<{ enabled: boolean; min_scope: MinScope }>) =>
    setDraft((d) => ({
      ...d,
      tools: { ...d.tools, [t.name]: { enabled: enabledOf(t), min_scope: scopeOf(t), ...patch } },
    }));

  const setMany = (list: McpToolInfo[], enabled: boolean) =>
    setDraft((d) => {
      const next = { ...d.tools };
      for (const t of list) {
        if (enabled && t.name === ESCAPE_HATCH) continue; // never bulk-enable the escape hatch
        next[t.name] = { enabled, min_scope: (next[t.name]?.min_scope as MinScope | undefined) ?? defaultScope(t) };
      }
      return { ...d, tools: next };
    });

  const groups = useMemo(() => {
    const needle = q.trim().toLowerCase();
    const m = new Map<string, McpToolInfo[]>();
    for (const t of tools) {
      const st = stats.get(t.name);
      const hay = `${t.name} ${t.title ?? ''} ${t.description ?? ''}`.toLowerCase();
      if (needle && !hay.includes(needle)) continue;
      const on = enabledOf(t);
      if (filter === 'enabled' && !on) continue;
      if (filter === 'disabled' && on) continue;
      if (filter === 'destructive' && !t.destructive) continue;
      if (filter === 'read_only' && !t.read_only) continue;
      if (filter === 'errors' && !(st && st.errors_24h > 0)) continue;
      if (filter === 'used' && !(st && st.calls_24h > 0)) continue;
      const c = categoryOf(t);
      m.set(c, [...(m.get(c) ?? []), t]);
    }
    for (const [k, l] of m) m.set(k, [...l].sort((a, b) => a.name.localeCompare(b.name)));
    return [...m.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  }, [tools, q, filter, stats, enabledOf]);

  const dirty = JSON.stringify(draft) !== JSON.stringify(saved);
  const enabledCount = tools.filter(enabledOf).length;
  const totals = [...stats.values()].reduce((a, s) => ({ calls: a.calls + s.calls_24h, errors: a.errors + s.errors_24h }), { calls: 0, errors: 0 });

  return (
    <div className="card">
      <div className="table-toolbar">
        <div className="card-title" style={{ marginBottom: 0 }}>
          MCP tools
        </div>
        <span className="faint">
          {enabledCount} of {tools.length} enabled
          {statsAvailable && `, ${totals.calls.toLocaleString()} calls and ${totals.errors.toLocaleString()} errors in 24 h`}
        </span>
      </div>
      {showHttp && (
      <div className="settings-list">
        <div className="settings-row">
          <div className="settings-row-text">
            <div className="settings-row-label">Streamable HTTP transport</div>
            <div className="settings-row-desc">Lets remote agents connect at {info.http_url}. When off, /mcp answers 404; local stdio agents keep working.</div>
          </div>
          <Toggle checked={draft.http_enabled} onChange={(v) => setDraft((d) => ({ ...d, http_enabled: v }))} label="Streamable HTTP transport" />
        </div>
      </div>
      )}
      <div className="tool-toolbar">
        <input className="input" type="search" placeholder={`Search ${tools.length} tools`} aria-label="Search tools" value={q} onChange={(e) => setQ(e.target.value)} />
        <select value={filter} onChange={(e) => setFilter(e.target.value as ToolFilter)} aria-label="Filter tools">
          <option value="all">All tools</option>
          <option value="enabled">Enabled</option>
          <option value="disabled">Disabled</option>
          <option value="destructive">Destructive</option>
          <option value="read_only">Read-only</option>
          {statsAvailable && <option value="used">Called in 24 h</option>}
          {statsAvailable && <option value="errors">With errors in 24 h</option>}
        </select>
        <button className="btn btn-sm" onClick={() => setCollapsed(new Set(groups.map(([c]) => c)))}>
          Collapse all
        </button>
        <button className="btn btn-sm" onClick={() => setCollapsed(new Set())}>
          Expand all
        </button>
      </div>
      {tools.length === 0 ? (
        <div className="muted">This server did not list any MCP tools.</div>
      ) : groups.length === 0 ? (
        <div className="muted">No tool matches these filters.</div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl tool-tbl">
            <thead>
              <tr>
                <th style={{ width: 56 }}>On</th>
                <th>Tool</th>
                <th>Kind</th>
                <th>Minimum scope</th>
                {statsAvailable && <th className="num">Calls 24 h</th>}
                {statsAvailable && <th className="num">Errors 24 h</th>}
                {statsAvailable && <th>Last call</th>}
              </tr>
            </thead>
            <tbody>
              {groups.map(([cat, list]) => {
                const isCollapsed = collapsed.has(cat);
                const on = list.filter(enabledOf).length;
                return (
                  <Fragment key={cat}>
                    <tr className="group-row tool-group">
                      <td colSpan={statsAvailable ? 7 : 4}>
                        <div className="tool-group-head">
                          <button
                            type="button"
                            className="group-toggle"
                            aria-expanded={!isCollapsed}
                            onClick={() =>
                              setCollapsed((cur) => {
                                const n = new Set(cur);
                                if (n.has(cat)) n.delete(cat);
                                else n.add(cat);
                                return n;
                              })
                            }
                          >
                            <span className={`caret${isCollapsed ? '' : ' open'}`} aria-hidden="true">
                              ▸
                            </span>
                            {cat}
                          </button>
                          <span className="faint">
                            {on}/{list.length} on
                          </span>
                          <div className="spacer" />
                          <button className="btn btn-sm btn-ghost" onClick={() => setMany(list, true)}>
                            Enable group
                          </button>
                          <button className="btn btn-sm btn-ghost" onClick={() => setMany(list, false)}>
                            Disable group
                          </button>
                        </div>
                      </td>
                    </tr>
                    {!isCollapsed &&
                      list.map((t) => {
                        const st = stats.get(t.name);
                        const enabled = enabledOf(t);
                        const escape = t.name === ESCAPE_HATCH;
                        return (
                          <tr key={t.name} className={`${enabled ? '' : 'tool-off'}${escape ? ' tool-escape' : ''}`}>
                            <td>
                              <Toggle
                                checked={enabled}
                                label={`Enable ${t.name}`}
                                onChange={(v) => {
                                  if (v && escape && !window.confirm('Enable the raw API tool? It lets an agent call any /api route with admin scope. Only do this for a trusted agent.')) return;
                                  setTool(t, { enabled: v });
                                }}
                              />
                            </td>
                            <td>
                              <div className="mono-sm tool-name">{t.name}</div>
                              {(t.title || t.description) && <div className="tool-desc">{t.title && t.title !== t.name ? t.title : t.description}</div>}
                              {escape && (
                                <div className="tool-warn">
                                  Raw access to every /api/v1 and /api/v2 route with admin scope. Keep it off unless a trusted agent needs something no other tool covers.
                                </div>
                              )}
                            </td>
                            <td>
                              <span className="badge-row">
                                {t.read_only && <span className="kbadge kb-read">read-only</span>}
                                {t.destructive && <span className="kbadge kb-danger">destructive</span>}
                                {!t.read_only && !t.destructive && <span className="kbadge">changes data</span>}
                              </span>
                            </td>
                            <td>
                              <select value={scopeOf(t)} onChange={(e) => setTool(t, { min_scope: e.target.value as MinScope })} aria-label={`Minimum scope for ${t.name}`}>
                                {MIN_SCOPES.map((s) => (
                                  <option key={s} value={s}>
                                    {s}
                                  </option>
                                ))}
                              </select>
                            </td>
                            {statsAvailable && <td className="num">{st ? st.calls_24h.toLocaleString() : 0}</td>}
                            {statsAvailable && <td className={`num${st && st.errors_24h > 0 ? ' val-bad' : ''}`}>{st ? st.errors_24h.toLocaleString() : 0}</td>}
                            {statsAvailable && <td className="muted">{st?.last_called_at ? fmtDate(st.last_called_at) : '—'}</td>}
                          </tr>
                        );
                      })}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      <div className="save-bar">
        <span className="faint">{dirty ? 'Unsaved tool changes' : 'Policy saved. Agents pick up changes within a minute.'}</span>
        <div className="spacer" />
        <button className="btn btn-sm" disabled={!dirty || busy} onClick={() => setDraft(saved)}>
          Reset
        </button>
        <button
          className="btn btn-sm btn-primary"
          disabled={!dirty || busy}
          onClick={() => {
            setBusy(true);
            void savePolicy(draft, 'MCP policy saved').finally(() => setBusy(false));
          }}
        >
          {busy ? 'Saving…' : 'Save policy'}
        </button>
      </div>
    </div>
  );
}

/** Read-only tool list for non-admins or servers without the policy API. */
function ToolList({ tools }: { tools: McpToolInfo[] }) {
  if (tools.length === 0) return null;
  return (
    <div className="card">
      <div className="card-title">MCP tools ({tools.length})</div>
      <div className="tbl-wrap">
        <table className="tbl">
          <tbody>
            {[...tools]
              .sort((a, b) => categoryOf(a).localeCompare(categoryOf(b)) || a.name.localeCompare(b.name))
              .map((t) => (
                <tr key={t.name} className={t.enabled === false ? 'tool-off' : ''}>
                  <td className="muted nowrap">{categoryOf(t)}</td>
                  <td className="mono-sm nowrap">{t.name}</td>
                  <td className="muted">{t.description ?? ''}</td>
                </tr>
              ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

export interface ApiMcpPanelProps {
  /** v2.4: scope tokens and the MCP policy to this workspace. */
  workspaceId?: string;
  /** Workspace admin (or platform admin): may create tokens and edit the workspace's MCP policy. */
  canManage?: boolean;
  /** Settings → API & MCP on a v2.4 server: only the connection info (tokens moved to Workspace). */
  connectionOnly?: boolean;
}

export function ApiMcpPanel({ workspaceId, canManage, connectionOnly }: ApiMcpPanelProps = {}) {
  const toast = useToast();
  const { user } = useUser();
  const isAdmin = user?.role === 'admin';
  const scoped = workspaceId !== undefined;
  const canPolicy = scoped ? canManage === true : isAdmin;
  const [info, setInfo] = useState<McpInfo | null>(null);
  const [infoState, setInfoState] = useState<'loading' | 'ok' | 'missing'>('loading');
  const [policy, setPolicy] = useState<McpPolicy | null>(null);
  const [policyMissing, setPolicyMissing] = useState(false);
  const [stats, setStats] = useState<Map<string, McpToolStat>>(new Map());
  const [statsAvailable, setStatsAvailable] = useState(false);

  useEffect(() => {
    let alive = true;
    fetchMcpInfo()
      .then((i) => {
        if (!alive) return;
        setInfo(i);
        setInfoState('ok');
      })
      .catch(() => alive && setInfoState('missing'));
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    if (connectionOnly || !canPolicy || infoState === 'loading') return;
    let alive = true;
    setPolicy(null);
    setPolicyMissing(false);
    const load: Promise<Partial<McpPolicy>> = scoped && workspaceId ? fetchWorkspaceMcpPolicy(workspaceId) : fetchMcpPolicy();
    load.then((p) => alive && setPolicy(normPolicy(p, info))).catch(() => alive && setPolicyMissing(true));
    if (isAdmin) {
      fetchMcpStats()
        .then((s) => {
          if (!alive) return;
          setStats(new Map(s.map((x) => [x.name, x])));
          setStatsAvailable(true);
        })
        .catch(() => {
          /* stats are optional */
        });
    }
    return () => {
      alive = false;
    };
  }, [connectionOnly, canPolicy, isAdmin, infoState, info, scoped, workspaceId]);

  const savePolicy = useCallback(
    async (p: McpPolicy, msg: string) => {
      try {
        const next: Partial<McpPolicy> | null =
          scoped && workspaceId ? await putWorkspaceMcpPolicy(workspaceId, p) : await putMcpPolicy(p);
        setPolicy(normPolicy(next && typeof next === 'object' && 'tools' in next ? next : p, info));
        toast.push(msg, 'success');
        return true;
      } catch (e) {
        toast.push(v2ErrorMessage(e, 'Could not save the MCP policy'), 'error');
        return false;
      }
    },
    [toast, info, scoped, workspaceId],
  );

  const tools = info?.tools ?? [];

  if (connectionOnly) {
    if (infoState === 'loading') return <Skeleton rows={3} />;
    if (infoState === 'ok') return <McpPanel showTools={false} />;
    return (
      <div className="card">
        <FeatureUnavailable feature="The MCP server" />
      </div>
    );
  }

  return (
    <>
      <TokensCard
        isAdmin={isAdmin}
        canPolicy={canPolicy}
        canCreate={scoped ? canManage === true : true}
        workspaceId={workspaceId}
        tools={tools}
        policy={policy}
        savePolicy={savePolicy}
      />
      {infoState === 'loading' ? (
        <Skeleton rows={4} />
      ) : infoState === 'missing' ? (
        <div className="card">
          <FeatureUnavailable feature="The MCP server" />
        </div>
      ) : info && canPolicy && policy ? (
        <ToolManager info={info} saved={policy} stats={stats} statsAvailable={statsAvailable} savePolicy={savePolicy} showHttp={!scoped} />
      ) : (
        <>
          {canPolicy && policyMissing && (
            <div className="info-note">
              <span className="info-note-text">Tool management is not available on this server. The tools below are shown read-only.</span>
            </div>
          )}
          <ToolList tools={tools} />
        </>
      )}
      {infoState === 'ok' && !scoped && <McpPanel showTools={false} />}
    </>
  );
}
