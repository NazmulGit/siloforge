import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { useMatch, useNavigate } from 'react-router-dom';
import { fetchSites } from './api';
import { readRememberedSite, writeRememberedSite } from './activeSite';
import { SitePickerModal } from './components/SitePickerModal';
import type { SiteListItem } from './types';

interface SitesCtx {
  sites: SiteListItem[];
  loading: boolean;
  error: string | null;
  reload: () => Promise<void>;
  /** Site in the URL, else the remembered site if it still exists, else the only site. */
  currentSiteId: string | null;
  rememberSite: (id: string) => void;
  /** Open the site picker; after a pick, navigate to /site/:id/<seg> (or just remember it). */
  pickSite: (seg?: string) => void;
}

const Ctx = createContext<SitesCtx | null>(null);

export function SitesProvider({ children }: { children: ReactNode }) {
  const [sites, setSites] = useState<SiteListItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [remembered, setRemembered] = useState<string | null>(readRememberedSite);
  const [picker, setPicker] = useState<{ seg?: string } | null>(null);
  const navigate = useNavigate();
  const match = useMatch('/site/:siteId/*');
  const urlSite = match?.params.siteId ?? null;

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

  const rememberSite = useCallback((id: string) => {
    setRemembered(id);
    writeRememberedSite(id);
  }, []);

  // Visiting /site/:id/... makes that the remembered site.
  useEffect(() => {
    if (urlSite && urlSite !== remembered) rememberSite(urlSite);
  }, [urlSite, remembered, rememberSite]);

  const currentSiteId = useMemo(() => {
    if (urlSite) return urlSite;
    if (remembered && sites.some((s) => s.id === remembered)) return remembered;
    if (sites.length === 1) return sites[0]?.id ?? null;
    return null;
  }, [urlSite, remembered, sites]);

  const pickSite = useCallback((seg?: string) => setPicker({ seg }), []);

  const value = useMemo(
    () => ({ sites, loading, error, reload, currentSiteId, rememberSite, pickSite }),
    [sites, loading, error, reload, currentSiteId, rememberSite, pickSite],
  );

  return (
    <Ctx.Provider value={value}>
      {children}
      {picker && (
        <SitePickerModal
          sites={sites}
          loading={loading}
          currentId={currentSiteId}
          onClose={() => setPicker(null)}
          onPick={(id) => {
            rememberSite(id);
            const seg = picker.seg;
            setPicker(null);
            if (seg) navigate(`/site/${id}/${seg}`);
          }}
        />
      )}
    </Ctx.Provider>
  );
}

export function useSites(): SitesCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useSites must be used inside <SitesProvider>');
  return ctx;
}
