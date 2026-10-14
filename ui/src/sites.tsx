import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { fetchSites } from './api';
import type { SiteListItem } from './types';

interface SitesCtx {
  sites: SiteListItem[];
  loading: boolean;
  error: string | null;
  reload: () => Promise<void>;
}

const Ctx = createContext<SitesCtx | null>(null);

export function SitesProvider({ children }: { children: ReactNode }) {
  const [sites, setSites] = useState<SiteListItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setSites(await fetchSites());
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load sites');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  const value = useMemo(() => ({ sites, loading, error, reload }), [sites, loading, error, reload]);

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useSites(): SitesCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useSites must be used inside <SitesProvider>');
  return ctx;
}
