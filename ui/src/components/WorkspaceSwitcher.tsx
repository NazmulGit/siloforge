import { useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { isPlatformAdmin } from '../api';
import { useUser } from '../user';
import { useWorkspace } from '../workspaces';
import { ALL_WORKSPACES } from '../wsStore';
import { Icon } from './Icon';

/**
 * Compact top-bar workspace switcher. Always shown (single-user installs see "Default").
 * Platform admins also get "All workspaces" (no scope header, the pre-2.4 view) and can
 * browse every workspace on the platform.
 */
export function WorkspaceSwitcher() {
  const { user } = useUser();
  const ws = useWorkspace();
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState('');
  const box = useRef<HTMLDivElement>(null);
  const platform = isPlatformAdmin(user);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (box.current && !box.current.contains(e.target as Node)) setOpen(false);
    };
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && setOpen(false);
    document.addEventListener('mousedown', close);
    document.addEventListener('keydown', esc);
    return () => {
      document.removeEventListener('mousedown', close);
      document.removeEventListener('keydown', esc);
    };
  }, [open]);

  const others = useMemo(() => {
    const mineIds = new Set(ws.mine.map((w) => w.id));
    const needle = q.trim().toLowerCase();
    return ws.all
      .filter((w) => !mineIds.has(w.id))
      .filter((w) => !needle || w.name.toLowerCase().includes(needle) || w.slug.toLowerCase().includes(needle));
  }, [ws.all, ws.mine, q]);

  if (!ws.available) return null;

  const label = ws.viewAs
    ? ws.viewAs.name
    : ws.currentId === ALL_WORKSPACES
      ? 'All workspaces'
      : (ws.current?.name ?? (ws.loading ? '…' : 'Default'));

  function pick(id: string) {
    setOpen(false);
    setQ('');
    if (id !== ws.currentId || ws.viewAs) ws.switchTo(id);
  }

  return (
    <div className="ws-switch" ref={box}>
      <button
        type="button"
        className={`ws-switch-btn${ws.viewAs ? ' viewas' : ''}`}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-label={`Workspace: ${label}. Switch workspace`}
        data-hint="The workspace you are working in. Sites, jobs, tokens and limits belong to a workspace."
        onClick={() => setOpen((v) => !v)}
      >
        <span className="ws-mark" aria-hidden="true">
          {label.slice(0, 1).toUpperCase()}
        </span>
        <span className="ws-switch-name">{label}</span>
        <Icon name="chevron" size={13} />
      </button>
      {open && (
        <div className="ws-menu" role="listbox" aria-label="Workspaces">
          <div className="ws-menu-head">Your workspaces</div>
          {ws.mine.length === 0 && <div className="ws-menu-empty">You are not a member of any workspace.</div>}
          {ws.mine.map((w) => (
            <button
              key={w.id}
              type="button"
              role="option"
              aria-selected={w.id === ws.currentId && !ws.viewAs}
              className={`ws-item${w.id === ws.currentId && !ws.viewAs ? ' active' : ''}`}
              onClick={() => pick(w.id)}
            >
              <span className="ws-item-name">
                {w.name}
                {w.status === 'suspended' && <span className="ws-flag">suspended</span>}
              </span>
              <span className="ws-item-meta">
                {w.role} · {w.plan?.name ?? 'plan'}
              </span>
            </button>
          ))}
          {platform && (
            <>
              <div className="ws-menu-head">Platform</div>
              <button
                type="button"
                role="option"
                aria-selected={ws.currentId === ALL_WORKSPACES}
                className={`ws-item${ws.currentId === ALL_WORKSPACES && !ws.viewAs ? ' active' : ''}`}
                onClick={() => pick(ALL_WORKSPACES)}
              >
                <span className="ws-item-name">All workspaces</span>
                <span className="ws-item-meta">every site on this server</span>
              </button>
              {ws.all.length > ws.mine.length && (
                <>
                  <input
                    className="input ws-search"
                    type="search"
                    placeholder="Find a workspace"
                    aria-label="Find a workspace"
                    value={q}
                    onChange={(e) => setQ(e.target.value)}
                  />
                  <div className="ws-menu-scroll">
                    {others.slice(0, 50).map((w) => (
                      <button
                        key={w.id}
                        type="button"
                        role="option"
                        aria-selected={w.id === ws.currentId}
                        className={`ws-item${w.id === ws.currentId && !ws.viewAs ? ' active' : ''}`}
                        onClick={() => pick(w.id)}
                      >
                        <span className="ws-item-name">{w.name}</span>
                        <span className="ws-item-meta">
                          {w.slug} · {w.plan?.name ?? 'plan'}
                          {w.status !== 'active' ? ` · ${w.status}` : ''}
                        </span>
                      </button>
                    ))}
                    {others.length === 0 && <div className="ws-menu-empty">No match.</div>}
                  </div>
                </>
              )}
            </>
          )}
          <div className="ws-menu-foot">
            <Link to="/workspace" onClick={() => setOpen(false)}>
              Workspace settings
            </Link>
            {platform && (
              <Link to="/admin?tab=workspaces" onClick={() => setOpen(false)}>
                Manage all
              </Link>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
