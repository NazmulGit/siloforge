import { lazy, Suspense, useEffect, useState } from 'react';
import { BrowserRouter, Navigate, Outlet, Route, Routes, useLocation, useParams } from 'react-router-dom';
import { getToken } from './api';
import { ToastProvider } from './toast';
import { SitesProvider } from './sites';
import { DocsProvider } from './docs';
import { UserProvider } from './user';
import { BrandingProvider } from './branding';
import { WorkspaceProvider } from './workspaces';
import { PlanLimitHost, VerifyEmailBanner, ViewAsBanner } from './components/SaasNotices';
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
const LinkRunPage = lazy(() => import('./pages/LinkRun').then((m) => ({ default: m.LinkRunPage })));
const WorkspacePage = lazy(() => import('./pages/Workspace').then((m) => ({ default: m.WorkspacePage })));
const AdminPage = lazy(() => import('./pages/Admin').then((m) => ({ default: m.AdminPage })));
const ForgotPage = lazy(() => import('./pages/AuthFlows').then((m) => ({ default: m.ForgotPage })));
const ResetPage = lazy(() => import('./pages/AuthFlows').then((m) => ({ default: m.ResetPage })));
const VerifyEmailPage = lazy(() => import('./pages/AuthFlows').then((m) => ({ default: m.VerifyEmailPage })));
const InvitePage = lazy(() => import('./pages/AuthFlows').then((m) => ({ default: m.InvitePage })));
const SettingsPage = lazy(() => import('./pages/Settings').then((m) => ({ default: m.SettingsPage })));

/** `/sites/:id/link-run` (spec §7 path) → the app's `/site/:id/link-run` route. */
function LinkRunAlias() {
  const { siteId } = useParams<{ siteId: string }>();
  const { search } = useLocation();
  return <Navigate to={`/site/${siteId ?? ''}/link-run${search}`} replace />;
}

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
    <WorkspaceProvider>
      <SitesProvider>
        <DocsProvider>
          <div className="layout">
            <Sidebar open={menuOpen} onNavigate={() => setMenuOpen(false)} />
            {menuOpen && <div className="sidebar-scrim" onClick={() => setMenuOpen(false)} aria-hidden="true" />}
            <div className="main">
              <Topbar onMenu={() => setMenuOpen((v) => !v)} />
              <ViewAsBanner />
              <VerifyEmailBanner />
              <main className="content">
                <Suspense fallback={<RouteFallback />}>
                  <Outlet />
                </Suspense>
              </main>
            </div>
          </div>
        </DocsProvider>
      </SitesProvider>
    </WorkspaceProvider>
  );
}

export default function App() {
  return (
    <BrowserRouter>
      <ToastProvider>
        <BrandingProvider>
        <UserProvider>
          <PlanLimitHost />
          <Suspense fallback={<RouteFallback />}>
            <Routes>
              <Route path="/login" element={<LoginPage />} />
              <Route path="/register" element={<RegisterPage />} />
              <Route path="/forgot" element={<ForgotPage />} />
              <Route path="/reset-password" element={<ResetPage />} />
              <Route path="/verify-email" element={<VerifyEmailPage />} />
              <Route path="/invite/:token" element={<InvitePage />} />
              <Route element={<RequireAuth />}>
                <Route element={<Layout />}>
                  <Route path="/" element={<Navigate to="/sites" replace />} />
                  <Route path="/sites" element={<SitesPage />} />
                  <Route path="/settings" element={<SettingsPage />} />
                  <Route path="/workspace" element={<WorkspacePage />} />
                  <Route path="/admin" element={<AdminPage />} />
                  <Route path="/system" element={<SystemPage />} />
                  <Route path="/jobs" element={<AllJobsPage />} />
                  <Route path="/logs" element={<LogsPage />} />
                  <Route path="/downloads" element={<DownloadsPage />} />
                  <Route path="/docs" element={<DocsPage />} />
                  <Route path="/docs/:slug" element={<DocsPage />} />
                  <Route path="/site/:siteId/dashboard" element={<DashboardPage />} />
                  <Route path="/site/:siteId/tools" element={<ToolsPage />} />
                  <Route path="/site/:siteId/link-run" element={<LinkRunPage />} />
                  <Route path="/sites/:siteId/link-run" element={<LinkRunAlias />} />
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
        </BrandingProvider>
      </ToastProvider>
    </BrowserRouter>
  );
}
