import { useEffect } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { useSites } from '../sites';
import { ACTIVE_SITE_KEY } from '../activeSite';

export function SiteSelect({ siteId }: { siteId?: string }) {
  const { sites, loading } = useSites();
  const navigate = useNavigate();
  const location = useLocation();

  const sectionMatch = location.pathname.match(/^\/site\/[^/]+\/(\w+)/);
  const section = sectionMatch?.[1] ?? 'dashboard';

  // Remember the active site so the global /settings page can scope its
  // site-specific panels to the last site the user was looking at.
  useEffect(() => {
    if (!siteId) return;
    try {
      localStorage.setItem(ACTIVE_SITE_KEY, siteId);
    } catch {
      /* storage unavailable — ignore */
    }
  }, [siteId]);

  return (
    <select
      className="site-select"
      aria-label="Selected site"
      data-hint="Switch the active site. Every page (Dashboard, Tools, Reports…) then shows data for the site you pick."
      value={siteId ?? ''}
      onChange={(e) => {
        const id = e.target.value;
        if (id) {
          try {
            localStorage.setItem(ACTIVE_SITE_KEY, id);
          } catch {
            /* storage unavailable — ignore */
          }
          navigate(`/site/${id}/${section}`);
        }
      }}
    >
      <option value="" disabled>
        {loading ? 'Loading sites…' : sites.length ? 'Select a site…' : 'No sites yet'}
      </option>
      {sites.map((s) => (
        <option key={s.id} value={s.id}>
          {s.domain}
        </option>
      ))}
    </select>
  );
}
