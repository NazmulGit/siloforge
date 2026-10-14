import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { clearToken, fetchMe, getToken, logoutSession, type AuthUser } from './api';
import { writeViewAs } from './wsStore';

interface UserCtx {
  user: AuthUser | null;
  setUser: (u: AuthUser | null) => void;
  /** Re-read /auth/me (after profile, email or 2FA changes). */
  refresh: () => Promise<void>;
  /** Revokes the session server-side (v2.4), then drops the token. */
  logout: () => Promise<void>;
}

const Ctx = createContext<UserCtx | null>(null);

export function UserProvider({ children }: { children: ReactNode }) {
  const [user, setUser] = useState<AuthUser | null>(null);

  const refresh = useCallback(async () => {
    if (!getToken()) return;
    try {
      setUser(await fetchMe());
    } catch {
      /* 401s are handled globally by the api wrapper */
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const logout = useCallback(async () => {
    await logoutSession();
    clearToken();
    writeViewAs(null);
    setUser(null);
  }, []);

  const value = useMemo<UserCtx>(() => ({ user, setUser, refresh, logout }), [user, refresh, logout]);

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useUser(): UserCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useUser must be used inside <UserProvider>');
  return ctx;
}
