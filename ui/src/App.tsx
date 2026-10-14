import { lazy, Suspense, useEffect, useState } from 'react';
import { BrowserRouter, Navigate, Outlet, Route, Routes, useLocation } from 'react-router-dom';
import { getToken } from './api';
import { ToastProvider } from './toast';
import { SitesProvider } from './sites';
import { DocsProvider } from './docs';
import { UserProvider } from './user';
import { Sidebar } from './components/Sidebar';
import { Topbar } from './components/Topbar';

// Keep graph visualization, history tables, and the other route-specific code out of the
// initial bundle. Reports is by far the heaviest page, so loading it only when requested
// materially lowers the browser CPU and memory cost for every other section.
const LoginPage = lazy(() => import('./pages/Login').then((m) => ({ default: m.LoginPage })));
const RegisterPage = lazy(() => import('./pages/Register').then((m) => ({ default: m.RegisterPage })));
const SitesPage = lazy(() => import('./pages/Sites').then((m) => ({ default: m.SitesPage })));
const DashboardPage = lazy(() => import('./pages/Dashboard').then((m) => ({ default: m.DashboardPage })));
const ToolsPage = lazy(() => import('./pages/Tools').then((m) => ({ default: m.ToolsPage })));
const ReportsPage = lazy(() => import('./pages/Reports').then((m) => ({ default: m.ReportsPage })));
const LinkHealthPage = lazy(() => import('./pages/LinkHealth').then((m) => ({ default: m.LinkHealthPage })));
const HistoryPage = lazy(() => import('./pages/History').then((m) => ({ default: m.HistoryPage })));
const IntegrationsPage = lazy(() => import('./pages/Integrations').then((m) => ({ default: m.IntegrationsPage })));
const JobsPage = lazy(() => import('./pages/Jobs').then((m) => ({ default: m.JobsPage })));
const SystemPage = lazy(() => import('./pages/System').then((m) => ({ default: m.SystemPage })));
const AllJobsPage = lazy(() => import('./pages/AllJobs').then((m) => ({ default: m.AllJobsPage })));
const DownloadsPage = lazy(() => import('./pages/Downloads').then((m) => ({ default: m.DownloadsPage })));
const DocsPage = lazy(() => import('./pages/Docs').then((m) => ({ default: m.DocsPage })));
const LogsPage = lazy(() => import('./pages/Logs').then((m) => ({ default: m.LogsPage })));
const SettingsPage = lazy(() => import('./pages/Settings').then((m) => ({ default: m.SettingsPage })));

function RouteFallback() {
  return <div className="route-loading" role="status">Loading…</div>;
}

function RequireAuth() {
  if (!getToken()) return <Navigate to="/login" replace />;
  return <Outlet />;
}

function Layout() {
  const [menuOpen, setMenuOpen] = useState(false);
  const location = useLocation();
  // Close the small-screen menu after every navigation.
  useEffect(() => setMenuOpen(false), [location.pathname, location.search]);
  return (
    <SitesProvider>
      <DocsProvider>
        <div className="layout">
          <Sidebar open={menuOpen} onNavigate={() => setMenuOpen(false)} />
          {menuOpen && <div className="sidebar-scrim" onClick={() => setMenuOpen(false)} aria-hidden="true" />}
          <div className="main">
            <Topbar onMenu={() => setMenuOpen((v) => !v)} />
            <main className="content">
              <Suspense fallback={<RouteFallback />}>
                <Outlet />
              </Suspense>
            </main>
          </div>
        </div>
      </DocsProvider>
    </SitesProvider>
  );
}

export default function App() {
  return (
    <BrowserRouter>
      <ToastProvider>
        <UserProvider>
          <Suspense fallback={<RouteFallback />}>
            <Routes>
              <Route path="/login" element={<LoginPage />} />
              <Route path="/register" element={<RegisterPage />} />
              <Route element={<RequireAuth />}>
                <Route element={<Layout />}>
                  <Route path="/" element={<Navigate to="/sites" replace />} />
                  <Route path="/sites" element={<SitesPage />} />
                  <Route path="/settings" element={<SettingsPage />} />
                  <Route path="/system" element={<SystemPage />} />
                  <Route path="/jobs" element={<AllJobsPage />} />
                  <Route path="/logs" element={<LogsPage />} />
                  <Route path="/downloads" element={<DownloadsPage />} />
                  <Route path="/docs" element={<DocsPage />} />
                  <Route path="/docs/:slug" element={<DocsPage />} />
                  <Route path="/site/:siteId/dashboard" element={<DashboardPage />} />
                  <Route path="/site/:siteId/tools" element={<ToolsPage />} />
                  <Route path="/site/:siteId/reports" element={<ReportsPage />} />
                  <Route path="/site/:siteId/link-health" element={<LinkHealthPage />} />
                  <Route path="/site/:siteId/history" element={<HistoryPage />} />
                  <Route path="/site/:siteId/integrations" element={<IntegrationsPage />} />
                  <Route path="/site/:siteId/jobs" element={<JobsPage />} />
                </Route>
              </Route>
              <Route path="*" element={<Navigate to="/sites" replace />} />
            </Routes>
          </Suspense>
        </UserProvider>
      </ToastProvider>
    </BrowserRouter>
  );
}
