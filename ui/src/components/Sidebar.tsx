import { NavLink } from 'react-router-dom';
import { useUser } from '../user';

const SITE_NAV: Array<{ label: string; seg: string; hint: string }> = [
  {
    label: 'Dashboard',
    seg: 'dashboard',
    hint: 'Overview of this site: link health score, orphan / dead-end counts, opportunities and recent jobs.',
  },
  {
    label: 'Tools',
    seg: 'tools',
    hint: 'Review, approve and inject candidate internal links — inbound, outbound, silo, bulk, anchors and keyword rules.',
  },
  {
    label: 'Reports',
    seg: 'reports',
    hint: 'Per-post link metrics (PageRank, click depth, link flow) and an interactive graph of the site.',
  },
  {
    label: 'Link Health',
    seg: 'link-health',
    hint: 'Broken links, redirect chains, internal links that carry rel=nofollow, and breadcrumb coverage.',
  },
  {
    label: 'History',
    seg: 'history',
    hint: 'Audit trail of every internal link that was actually applied or reverted, with CSV export.',
  },
  {
    label: 'Integrations',
    seg: 'integrations',
    hint: 'Import content from non-WordPress CMSes and export decided links — all as CSV.',
  },
  {
    label: 'Jobs',
    seg: 'jobs',
    hint: 'Live status of every run for this site, plus recurring schedules and metric trends.',
  },
];

function LogoGlyph() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="#2f6bff" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
      <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
    </svg>
  );
}

export function Sidebar({ siteId }: { siteId?: string }) {
  const { user } = useUser();
  const isAdmin = user?.role === 'admin';
  return (
    <aside className="sidebar">
      <div className="logo">
        <LogoGlyph />
        <span>
          SILO<b>FORGE</b>
        </span>
      </div>
      <nav>
        {SITE_NAV.map((item) =>
          siteId ? (
            <NavLink
              key={item.seg}
              to={`/site/${siteId}/${item.seg}`}
              className={({ isActive }) => `nav-link${isActive ? ' active' : ''}`}
              data-hint={item.hint}
            >
              {item.label}
            </NavLink>
          ) : (
            <span key={item.seg} className="nav-link disabled" title="Select a site first" data-hint={item.hint}>
              {item.label}
            </span>
          ),
        )}
        <NavLink
          to="/sites"
          className={({ isActive }) => `nav-link${isActive ? ' active' : ''}`}
          data-hint="Your connected sites. Add a new site, or start a crawl / plan for any of them."
        >
          Sites
        </NavLink>
        <NavLink
          to="/jobs"
          end
          className={({ isActive }) => `nav-link${isActive ? ' active' : ''}`}
          data-hint="Every job across all your sites: progress, resource use, logs and cancel."
        >
          All jobs
        </NavLink>
        {isAdmin && (
          <NavLink
            to="/system"
            className={({ isActive }) => `nav-link${isActive ? ' active' : ''}`}
            data-hint="Server health: CPU and memory over time, job queue and recent errors. Admins only."
          >
            System
          </NavLink>
        )}
        <NavLink
          to="/settings"
          className={({ isActive }) => `nav-link${isActive ? ' active' : ''}`}
          data-hint="App preferences — including the toggle for these hover hints — and your account info."
        >
          Settings
        </NavLink>
      </nav>
      <div className="sidebar-foot">
        SiloForge v{__APP_VERSION__}
        <br />
        self-hosted internal linking
      </div>
    </aside>
  );
}
