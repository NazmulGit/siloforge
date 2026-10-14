import { NavLink, useLocation, useSearchParams } from 'react-router-dom';
import { useSites } from '../sites';
import { useUser } from '../user';
import { Icon, type IconName } from './Icon';

interface SiteNavItem {
  label: string;
  seg: string;
  icon: IconName;
  hint: string;
}

export const SITE_NAV: SiteNavItem[] = [
  { label: 'Dashboard', seg: 'dashboard', icon: 'dashboard', hint: 'Overview of this site: link health score, orphan / dead-end counts, opportunities and recent jobs.' },
  { label: 'Tools', seg: 'tools', icon: 'tools', hint: 'Review, approve and inject candidate internal links — inbound, outbound, silo, bulk, anchors and keyword rules.' },
  { label: 'Reports', seg: 'reports', icon: 'reports', hint: 'Per-post link metrics (PageRank, click depth, link flow) and an interactive graph of the site.' },
  { label: 'Link Health', seg: 'link-health', icon: 'health', hint: 'Broken links, redirect chains, internal links that carry rel=nofollow, and breadcrumb coverage.' },
  { label: 'History', seg: 'history', icon: 'history', hint: 'Audit trail of every internal link that was actually applied or reverted, with CSV export.' },
  { label: 'Integrations', seg: 'integrations', icon: 'integrations', hint: 'Import content from non-WordPress CMSes and export decided links — all as CSV.' },
  { label: 'Jobs', seg: 'jobs', icon: 'jobs', hint: 'Live status of every run for this site, plus recurring schedules and metric trends.' },
];

function LogoGlyph() {
  return (
    <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
      <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
    </svg>
  );
}

const linkCls = ({ isActive }: { isActive: boolean }) => `nav-link${isActive ? ' active' : ''}`;

/** Settings shortcuts shown under the Settings group (query-param sections). */
const SETTINGS_LINKS: Array<{ label: string; section: string; icon: IconName; hint: string }> = [
  { label: 'Settings', section: '', icon: 'settings', hint: 'Every setting: linking rules, resources, WordPress, AI models, API & MCP, backups and more.' },
  { label: 'Resources', section: 'resources', icon: 'cpu', hint: 'CPU, RAM and GPU limits for this server. Admins only.' },
  { label: 'API & MCP', section: 'api', icon: 'key', hint: 'API tokens and their scopes, plus the MCP tool manager for Hermes Agent.' },
];

export function Sidebar({ open, onNavigate }: { open: boolean; onNavigate: () => void }) {
  const { user } = useUser();
  const { sites, currentSiteId, pickSite } = useSites();
  const location = useLocation();
  const [params] = useSearchParams();
  const isAdmin = user?.role === 'admin';
  const current = sites.find((s) => s.id === currentSiteId);
  const onSettings = location.pathname === '/settings';
  const section = params.get('section') ?? '';

  return (
    <aside className={`sidebar${open ? ' open' : ''}`} aria-label="Main menu">
      <div className="logo">
        <LogoGlyph />
        <span>
          Silo<b>Forge</b>
        </span>
      </div>
      <nav onClick={onNavigate}>
        <div className="nav-group">
          <div className="nav-group-head">
            <span>Site</span>
            <button
              type="button"
              className="nav-site-chip"
              title={current ? `Switch site (current: ${current.domain})` : 'Choose a site'}
              onClick={(e) => {
                e.stopPropagation();
                pickSite();
              }}
            >
              {current ? current.domain : 'choose…'}
            </button>
          </div>
          {SITE_NAV.map((item) =>
            currentSiteId ? (
              <NavLink key={item.seg} to={`/site/${currentSiteId}/${item.seg}`} className={linkCls} data-hint={item.hint}>
                <Icon name={item.icon} />
                {item.label}
              </NavLink>
            ) : (
              <button
                key={item.seg}
                type="button"
                className="nav-link"
                data-hint={`${item.hint} Pick a site to open it.`}
                onClick={() => pickSite(item.seg)}
              >
                <Icon name={item.icon} />
                {item.label}
              </button>
            ),
          )}
        </div>
        <div className="nav-group">
          <div className="nav-group-head">
            <span>Workspace</span>
          </div>
          <NavLink to="/sites" className={linkCls} data-hint="Your connected sites. Add a new site, or start a crawl / plan for any of them.">
            <Icon name="sites" />
            Sites
          </NavLink>
          <NavLink to="/jobs" end className={linkCls} data-hint="Every job across all your sites: progress, resource use, logs and cancel.">
            <Icon name="alljobs" />
            All jobs
          </NavLink>
          {isAdmin && (
            <NavLink to="/system" className={linkCls} data-hint="Server health: CPU and memory over time, job queue and recent errors. Admins only.">
              <Icon name="system" />
              System
            </NavLink>
          )}
          <NavLink to="/logs" className={linkCls} data-hint="One timeline of everything that happened: UI, API and MCP calls, jobs, sign-ins and alerts.">
            <Icon name="logs" />
            Logs
          </NavLink>
          <NavLink to="/downloads" className={linkCls} data-hint="Download the SiloForge app and the WordPress plugin, with versions and checksums.">
            <Icon name="downloads" />
            Downloads
          </NavLink>
          <NavLink to="/docs" className={linkCls} data-hint="The built-in user guide.">
            <Icon name="docs" />
            Docs
          </NavLink>
        </div>
        <div className="nav-group">
          <div className="nav-group-head">
            <span>Settings</span>
          </div>
          {SETTINGS_LINKS.filter((l) => l.section !== 'resources' || isAdmin).map((l) => {
            const active =
              onSettings && (l.section === '' ? !SETTINGS_LINKS.some((x) => x.section && x.section === section) : section === l.section);
            return (
              <NavLink
                key={l.label}
                to={l.section ? `/settings?section=${l.section}` : '/settings'}
                className={() => `nav-link${active ? ' active' : ''}`}
                data-hint={l.hint}
              >
                <Icon name={l.icon} />
                {l.label}
              </NavLink>
            );
          })}
        </div>
      </nav>
      <div className="sidebar-foot">SiloForge v{__APP_VERSION__}</div>
    </aside>
  );
}
