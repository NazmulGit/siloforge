// Admin console (v2.4 §4), platform admins only.

import { Navigate, useSearchParams } from 'react-router-dom';
import { isPlatformAdmin } from '../api';
import { AdminEmail, AdminInvites } from '../components/admin/AdminMail';
import { AdminOverview } from '../components/admin/AdminOverview';
import { AdminPlans } from '../components/admin/AdminPlans';
import { AdminBranding, AdminDeployment, AdminRegistration } from '../components/admin/AdminPlatform';
import { AdminUsers } from '../components/admin/AdminUsers';
import { AdminWorkspaces } from '../components/admin/AdminWorkspaces';
import { HelpButton } from '../docs';
import { useUser } from '../user';
import { LogsPage } from './Logs';

const TABS = [
  { id: 'overview', label: 'Overview', hint: 'The whole platform at a glance: users, workspaces, usage and health.' },
  { id: 'users', label: 'Users', hint: 'Every account: create, edit, reset passwords, sign out, disable and delete.' },
  { id: 'workspaces', label: 'Workspaces', hint: 'Every workspace: plan, usage, members, suspend or delete, and a read-only support view.' },
  { id: 'plans', label: 'Plans', hint: 'Plan limits that apply per workspace.' },
  { id: 'invites', label: 'Invites', hint: 'Open invitations across every workspace.' },
  { id: 'email', label: 'Email', hint: 'The SMTP server for account emails, a test send, and the outbox of links to pass on by hand.' },
  { id: 'registration', label: 'Registration', hint: 'Who can sign up, email verification, allowed domains and the plan new workspaces get.' },
  { id: 'branding', label: 'Branding', hint: 'Name, logo, colour, support address and legal links on the sign-in pages.' },
  { id: 'audit', label: 'Audit', hint: 'The event log for the whole platform, with the same filters as Logs.' },
  { id: 'deployment', label: 'Deployment', hint: 'Public address and reverse-proxy settings for running SiloForge as a service.' },
] as const;

type TabId = (typeof TABS)[number]['id'];

export function AdminPage() {
  const { user } = useUser();
  const [params, setParams] = useSearchParams();
  const raw = params.get('tab');
  const tab: TabId = TABS.some((t) => t.id === raw) ? (raw as TabId) : 'overview';

  if (user && !isPlatformAdmin(user)) return <Navigate to="/sites" replace />;

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Admin</h1>
          <div className="page-sub">{TABS.find((t) => t.id === tab)?.hint}</div>
        </div>
        <div className="spacer" />
        <HelpButton />
      </div>
      <div className="tabs tabs-scroll" role="tablist">
        {TABS.map((t) => (
          <button
            key={t.id}
            role="tab"
            aria-selected={tab === t.id}
            className={`tab${tab === t.id ? ' active' : ''}`}
            data-hint={t.hint}
            onClick={() => setParams(t.id === 'overview' ? {} : { tab: t.id })}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div className="tab-body">
      {tab === 'overview' && <AdminOverview />}
      {tab === 'users' && <AdminUsers />}
      {tab === 'workspaces' && <AdminWorkspaces />}
      {tab === 'plans' && <AdminPlans />}
      {tab === 'invites' && <AdminInvites />}
      {tab === 'email' && <AdminEmail />}
      {tab === 'registration' && <AdminRegistration />}
      {tab === 'branding' && <AdminBranding />}
      {tab === 'audit' && <LogsPage platform />}
      {tab === 'deployment' && <AdminDeployment />}
      </div>
    </div>
  );
}
