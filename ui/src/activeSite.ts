// Tracks the "last selected site" so global routes (e.g. /settings) can scope
// their per-site panels to whatever site the user was last looking at.
//
// The site is normally selected via the URL (/site/:siteId/...). The Topbar
// SiteSelect mirrors that selection into localStorage here; global pages read
// it back, validate it against the loaded site list, and fall back to the
// first available site.

import { useCallback, useEffect, useState } from 'react';
import type { SiteListItem } from './types';

export const ACTIVE_SITE_KEY = 'sf_active_site';

function readStored(): string | null {
  try {
    return localStorage.getItem(ACTIVE_SITE_KEY);
  } catch {
    return null;
  }
}

function writeStored(id: string): void {
  try {
    localStorage.setItem(ACTIVE_SITE_KEY, id);
  } catch {
    /* storage unavailable — ignore */
  }
}

/**
 * Resolves the active site for a global (non-site-scoped) route.
 * Returns the current siteId (or null when there are no sites) plus a setter.
 * Validates the stored id against the loaded list and defaults to the first site.
 */
export function useActiveSite(sites: SiteListItem[]): {
  siteId: string | null;
  setSiteId: (id: string) => void;
} {
  const [siteId, setSiteIdState] = useState<string | null>(readStored);

  useEffect(() => {
    if (sites.length === 0) return;
    const valid = siteId != null && sites.some((s) => s.id === siteId);
    if (!valid) {
      const first = sites[0];
      if (first) setSiteIdState(first.id);
    }
  }, [sites, siteId]);

  const setSiteId = useCallback((id: string) => {
    setSiteIdState(id);
    writeStored(id);
  }, []);

  return { siteId, setSiteId };
}
