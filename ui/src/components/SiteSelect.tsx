import { useLocation } from 'react-router-dom';
import { useSites } from '../sites';
import { sitePostCount } from '../types';
import { Icon } from './Icon';

/**
 * Top-bar site switcher. Shows the current (URL or remembered) site; clicking opens the
 * searchable site picker. On a site page the pick keeps the same section.
 */
export function SiteSelect() {
  const { sites, loading, currentSiteId, pickSite } = useSites();
  const location = useLocation();
  const section = /^\/site\/[^/]+\/([\w-]+)/.exec(location.pathname)?.[1];
  const current = sites.find((s) => s.id === currentSiteId);

  return (
    <button
      type="button"
      className="site-switch"
      aria-label={current ? `Current site ${current.domain}. Switch site` : 'Choose a site'}
      data-hint="Switch the active site. Site pages (Dashboard, Tools, Reports…) show data for the site you pick. Remembered on this device."
      onClick={() => pickSite(section)}
    >
      <Icon name="sites" />
      <span className="site-switch-text">
        <span className="site-switch-domain">
          {current ? current.domain : loading ? 'Loading sites…' : sites.length ? 'Choose a site' : 'No sites yet'}
        </span>
        {current && <span className="site-switch-meta">{sitePostCount(current).toLocaleString()} posts</span>}
      </span>
      <Icon name="chevron" size={14} />
    </button>
  );
}
