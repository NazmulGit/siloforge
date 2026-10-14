import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { clearToken, fetchMe, getToken, type AuthUser } from './api';

interface UserCtx {
  user: AuthUser | null;
  setUser: (u: AuthUser | null) => void;
  logout: () => void;
}

const Ctx = createContext<UserCtx | null>(null);

export function UserProvider({ children }: { children: ReactNode }) {
  const [user, setUser] = useState<AuthUser | null>(null);

  useEffect(() => {
    if (!getToken()) return;
    let cancelled = false;
    fetchMe()
      .then((u) => {
        if (!cancelled) setUser(u);
      })
      .catch(() => {
        /* 401s are handled globally by the api wrapper */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const value = useMemo<UserCtx>(
    () => ({
      user,
      setUser,
      logout: () => {
        clearToken();
        setUser(null);
      },
    }),
    [user],
  );

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useUser(): UserCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useUser must be used inside <UserProvider>');
  return ctx;
}
