// The "current site": the site in the URL (/site/:siteId/...) or, on global routes,
// the last site the user picked. Remembered in localStorage `sf_site` (v2.1 contract §7);
// the older `sf_active_site` key is still read once for continuity.

import { useCallback } from 'react';
import { useSites } from './sites';
import type { SiteListItem } from './types';

export const ACTIVE_SITE_KEY = 'sf_site';
const LEGACY_KEY = 'sf_active_site';

export function readRememberedSite(): string | null {
  try {
    return localStorage.getItem(ACTIVE_SITE_KEY) ?? localStorage.getItem(LEGACY_KEY);
  } catch {
    return null;
  }
}

export function writeRememberedSite(id: string): void {
  try {
    localStorage.setItem(ACTIVE_SITE_KEY, id);
  } catch {
    /* storage unavailable — ignore */
  }
}

export function clearRememberedSite(): void {
  try {
    localStorage.removeItem(ACTIVE_SITE_KEY);
    localStorage.removeItem(LEGACY_KEY);
  } catch {
    /* storage unavailable — ignore */
  }
}

/**
 * Resolves the active site for a global (non-site-scoped) route: the remembered site
 * when it still exists, otherwise the first site. `sites` is accepted for backwards
 * compatibility; the list comes from the SitesProvider.
 */
export function useActiveSite(_sites?: SiteListItem[]): {
  siteId: string | null;
  setSiteId: (id: string) => void;
} {
  const { sites, currentSiteId, rememberSite } = useSites();
  const siteId = currentSiteId ?? sites[0]?.id ?? null;
  const setSiteId = useCallback((id: string) => rememberSite(id), [rememberSite]);
  return { siteId, setSiteId };
}
