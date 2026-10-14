// Current workspace (v2.4): the list from GET /api/v2/workspaces, the selected id
// (localStorage sf_workspace → X-SiloForge-Workspace on every request), and the platform
// admin's read-only "view as" support mode (X-SiloForge-View-As).

import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { isPlatformAdmin } from './api';
import { fetchWorkspaces, type WorkspaceRow } from './saas';
import { useUser } from './user';
import {
  ALL_WORKSPACES,
  readViewAs,
  readWorkspaceId,
  writeViewAs,
  writeWorkspaceId,
  type ViewAs,
} from './wsStore';

interface WorkspaceCtx {
  /** Workspaces the user belongs to. */
  mine: WorkspaceRow[];
  /** Platform admins: every workspace (role platform_admin where not a member). */
  all: WorkspaceRow[];
  loading: boolean;
  /** false when the server has no workspace API (pre-2.4): everything behaves as before. */
  available: boolean;
  /** Selected id, ALL_WORKSPACES for a platform admin's "All workspaces", or null (none yet). */
  currentId: string | null;
  current: WorkspaceRow | null;
  /** The caller's role in the current workspace (platform_admin for platform admins outside it). */
  role: string | null;
  switchTo: (id: string) => void;
  viewAs: ViewAs | null;
  startViewAs: (v: ViewAs) => void;
  exitViewAs: () => void;
  reload: () => Promise<void>;
  /** Bumps whenever the request scope changes; data providers reload on it. */
  scopeKey: number;
}

const Ctx = createContext<WorkspaceCtx | null>(null);

export function WorkspaceProvider({ children }: { children: ReactNode }) {
  const { user } = useUser();
  const platformAdmin = isPlatformAdmin(user);
  const [mine, setMine] = useState<WorkspaceRow[]>([]);
  const [all, setAll] = useState<WorkspaceRow[]>([]);
  const [loading, setLoading] = useState(true);
  const [available, setAvailable] = useState(true);
  const [currentId, setCurrentId] = useState<string | null>(readWorkspaceId);
  const [viewAs, setViewAs] = useState<ViewAs | null>(readViewAs);
  const [scopeKey, setScopeKey] = useState(0);
  const navigate = useNavigate();
  const location = useLocation();

  const reload = useCallback(async () => {
    try {
      const own = await fetchWorkspaces(false);
      setMine(own.workspaces);
      let every: WorkspaceRow[] = [];
      if (platformAdmin) {
        every = (await fetchWorkspaces(true).catch(() => own)).workspaces;
        setAll(every);
      }
      setAvailable(true);
      // Pick a starting workspace once: the stored choice when it still exists, else the
      // server's current one. A platform admin on a multi-workspace platform starts on
      // "All workspaces" so the sites list looks exactly like before v2.4.
      const stored = readWorkspaceId();
      const known = [...own.workspaces, ...every];
      let next = stored;
      if (stored === ALL_WORKSPACES && !platformAdmin) next = null;
      if (next && next !== ALL_WORKSPACES && !known.some((w) => w.id === next)) next = null;
      if (!next) {
        if (platformAdmin && every.length > 1) next = ALL_WORKSPACES;
        else next = own.current_id ?? user?.default_workspace_id ?? own.workspaces[0]?.id ?? null;
      }
      if (next !== stored) {
        writeWorkspaceId(next);
        setScopeKey((k) => k + 1);
      }
      setCurrentId(next);
    } catch {
      // Pre-2.4 server (404) or a transient error: no workspace header, v1 behaviour.
      setAvailable(false);
    } finally {
      setLoading(false);
    }
  }, [platformAdmin, user?.default_workspace_id]);

  useEffect(() => {
    if (user) void reload();
  }, [user, reload]);

  const leaveSitePage = useCallback(() => {
    if (location.pathname.startsWith('/site/')) navigate('/sites');
  }, [location.pathname, navigate]);

  const switchTo = useCallback(
    (id: string) => {
      writeViewAs(null);
      setViewAs(null);
      writeWorkspaceId(id);
      setCurrentId(id);
      setScopeKey((k) => k + 1);
      leaveSitePage();
    },
    [leaveSitePage],
  );

  const startViewAs = useCallback(
    (v: ViewAs) => {
      writeViewAs(v);
      setViewAs(v);
      setScopeKey((k) => k + 1);
      navigate('/sites');
    },
    [navigate],
  );

  const exitViewAs = useCallback(() => {
    writeViewAs(null);
    setViewAs(null);
    setScopeKey((k) => k + 1);
    leaveSitePage();
  }, [leaveSitePage]);

  const current = useMemo(() => {
    if (!currentId || currentId === ALL_WORKSPACES) return null;
    return mine.find((w) => w.id === currentId) ?? all.find((w) => w.id === currentId) ?? null;
  }, [currentId, mine, all]);

  const role = useMemo(() => {
    if (current) {
      const own = mine.find((w) => w.id === current.id);
      if (own) return own.role;
    }
    return platformAdmin ? 'platform_admin' : (current?.role ?? null);
  }, [current, mine, platformAdmin]);

  const value = useMemo<WorkspaceCtx>(
    () => ({ mine, all, loading, available, currentId, current, role, switchTo, viewAs, startViewAs, exitViewAs, reload, scopeKey }),
    [mine, all, loading, available, currentId, current, role, switchTo, viewAs, startViewAs, exitViewAs, reload, scopeKey],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useWorkspace(): WorkspaceCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useWorkspace must be used inside <WorkspaceProvider>');
  return ctx;
}

/** Same as useWorkspace() but null outside the provider (login pages, tests). */
export function useOptionalWorkspace(): WorkspaceCtx | null {
  return useContext(Ctx);
}
