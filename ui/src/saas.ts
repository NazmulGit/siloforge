// v2.4 "SaaS readiness" API: auth v2, workspaces, members, invites, plans, billing and the
// platform admin console (docs/specs/v2.4-saas.md §9). Same fetch wrapper as api.ts, so
// every call carries the Bearer token and the workspace / view-as headers.

import { requestAt, type AuthResponse, type AuthUser, type RequestInitLite } from './api';
import type { McpPolicy } from './types2';

const V2 = '/api/v2';

function call<T>(path: string, init: RequestInitLite = {}): Promise<T> {
  return requestAt<T>(V2, path, init);
}

function qs(params: Record<string, string | number | boolean | undefined | null>): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null || v === '') continue;
    p.set(k, typeof v === 'boolean' ? (v ? '1' : '0') : String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : '';
}

const enc = encodeURIComponent;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type RegistrationMode = 'closed' | 'invite_only' | 'open';
export type WorkspaceRole = 'owner' | 'admin' | 'member' | 'viewer';
export type MemberRole = Exclude<WorkspaceRole, 'owner'>;

export interface Branding {
  app_name: string;
  logo_url: string;
  primary_color: string;
  support_email: string;
  terms_url: string;
  privacy_url: string;
}

export interface AuthConfig {
  registration_mode: RegistrationMode;
  require_email_verification: boolean;
  email_enabled: boolean;
  first_run: boolean;
  branding: Branding;
  version?: string;
  /** Set when the server has no v2.4 auth API (older version): v1 behaviour. */
  legacy?: boolean;
}

export type Delivery = 'smtp' | 'manual' | null;

export interface RegisterResult extends AuthResponse {
  workspace: { id: string; name: string } | null;
  email_verification_required?: boolean;
  verification_delivery?: Delivery;
}

export interface SessionRow {
  id: string;
  created_at: string;
  last_seen_at: string | null;
  expires_at: string | null;
  ip: string | null;
  user_agent: string | null;
  remember?: boolean;
  current?: boolean;
  revoked_at?: string | null;
}

export interface PlanLimits {
  sites: number | null;
  posts_per_site: number | null;
  link_runs_per_month: number | null;
  ai_tokens_per_month: number | null;
  members: number | null;
  api_tokens: number | null;
  schedules: number | null;
  auto_linker: boolean;
  mcp: boolean;
}

export interface Plan {
  id: string;
  name: string;
  limits: PlanLimits;
  price_label: string | null;
  public: boolean;
  sort: number;
}

export interface WorkspaceUsage {
  sites: number;
  posts_total: number;
  posts_max_site: number;
  link_runs_month: number;
  ai_tokens_month: number;
  members: number;
  pending_invites: number;
  api_tokens: number;
  schedules: number;
}

export interface UsageReport {
  workspace_id: string;
  month: string;
  plan: { id: string; name: string; price_label?: string | null };
  limits: PlanLimits;
  usage: WorkspaceUsage;
  history?: Array<{ month: string; sites: number; posts: number; link_runs: number; ai_tokens: number; members: number }>;
}

export interface WorkspaceRow {
  id: string;
  name: string;
  slug: string;
  role: WorkspaceRole | 'platform_admin';
  status: 'active' | 'suspended' | 'deleted' | string;
  is_default?: boolean;
  owner_user_id?: string | null;
  plan: { id: string; name: string };
  sites?: number;
  created_at?: string;
}

export interface WorkspaceDetail extends WorkspaceRow {
  my_role?: WorkspaceRole | 'platform_admin';
  limits?: PlanLimits;
  usage?: WorkspaceUsage;
  deleted_at?: string | null;
  purge_after?: string | null;
}

export interface MemberRow {
  user_id: string;
  email: string;
  name: string;
  role: WorkspaceRole;
  invited_by?: string | null;
  created_at: string;
}

export interface InviteRow {
  id: string;
  email: string;
  role: MemberRole;
  expires_at: string;
  accepted_at?: string | null;
  created_at?: string;
  invited_by?: string | null;
  workspace_id?: string;
  workspace_name?: string;
  /** Present for manual delivery (no SMTP), when the server keeps it. */
  link?: string | null;
}

export interface InviteInfo {
  valid: boolean;
  email: string;
  role: MemberRole;
  workspace_name: string;
  expires_at: string;
}

export interface BillingInfo {
  provider: 'none' | 'manual' | 'stripe' | string;
  plan: { id: string; name: string; price_label?: string | null } | null;
  limits: PlanLimits | null;
  upgrade: { contact: string; note: string } | null;
}

// ---- admin ----

export interface AdminOverview {
  users: { total: number; admins: number; disabled: number; unverified: number; with_2fa: number };
  workspaces: { total: number; suspended: number; deleted: number };
  sites: number;
  posts: number;
  runs_today: number;
  ai_tokens_month: number;
  errors_open: number;
  mail: { configured: boolean; outbox_pending: number };
  registration_mode: RegistrationMode;
  health: { version: string; uptime_secs: number; jobs: { running: number; queued: number } };
}

export interface AdminUser {
  id: string;
  email: string;
  name: string;
  role: 'admin' | 'user';
  created_at: string;
  email_verified_at: string | null;
  disabled_at: string | null;
  two_factor_enabled: boolean;
  last_login_at: string | null;
  sessions_active: number;
  workspaces: Array<{ id: string; name: string; role: WorkspaceRole }>;
}

export interface AdminWorkspace extends WorkspaceRow {
  owner?: { id: string; email: string; name?: string } | null;
  members?: number | MemberRow[];
  sites?: number;
  usage?: WorkspaceUsage;
  limits?: PlanLimits;
  deleted_at?: string | null;
  purge_after?: string | null;
}

export interface OutboxRow {
  id: string;
  to: string;
  subject: string;
  kind: string;
  link: string | null;
  created_at: string;
  sent_at: string | null;
  error: string | null;
}

export interface SmtpSettings {
  host: string;
  port: number;
  tls: 'starttls' | 'tls' | 'none';
  username: string;
  password: string;
  from_email: string;
  from_name: string;
}

export interface PlatformSettings {
  registration: {
    mode: RegistrationMode;
    require_email_verification: boolean;
    allowed_domains: string[];
    denied_domains: string[];
    rate_limit_per_hour: number;
    default_plan_id: string;
  };
  smtp: SmtpSettings;
  branding: Branding;
  deployment: { public_url: string; trust_proxy: boolean };
  billing: { provider: string };
}

export type PlatformPatch = { [K in keyof PlatformSettings]?: Partial<PlatformSettings[K]> };

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

export const DEFAULT_BRANDING: Branding = {
  app_name: 'SiloForge',
  logo_url: '',
  primary_color: '',
  support_email: '',
  terms_url: '',
  privacy_url: '',
};

/** Public auth config; an older server (no v2.4) yields the v2.3 behaviour. */
export async function fetchAuthConfig(): Promise<AuthConfig> {
  try {
    const c = await call<Partial<AuthConfig>>('/auth/config');
    return {
      registration_mode: c.registration_mode ?? 'closed',
      require_email_verification: c.require_email_verification === true,
      email_enabled: c.email_enabled === true,
      first_run: c.first_run === true,
      branding: { ...DEFAULT_BRANDING, ...(c.branding ?? {}) },
      version: c.version,
    };
  } catch {
    return {
      registration_mode: 'open',
      require_email_verification: false,
      email_enabled: false,
      first_run: false,
      branding: DEFAULT_BRANDING,
      legacy: true,
    };
  }
}

export function registerV2(body: {
  email: string;
  password: string;
  name?: string;
  invite_token?: string;
  workspace_name?: string;
  website?: string;
}): Promise<RegisterResult> {
  return call<RegisterResult>('/auth/register', { method: 'POST', body });
}

export function verifyEmail(token: string): Promise<{ ok: boolean; email: string }> {
  return call('/auth/verify-email', { method: 'POST', body: { token } });
}

export function resendVerification(): Promise<{ sent: boolean; delivery: Delivery }> {
  return call('/auth/verify-email/resend', { method: 'POST', body: {} });
}

export function forgotPassword(email: string): Promise<{ ok: boolean }> {
  return call('/auth/forgot', { method: 'POST', body: { email } });
}

export function resetPassword(token: string, password: string): Promise<{ ok: boolean }> {
  return call('/auth/reset', { method: 'POST', body: { token, password } });
}

export function changeEmail(newEmail: string, password: string): Promise<{ ok: boolean; pending_email: string; delivery: Delivery }> {
  return call('/auth/change-email', { method: 'POST', body: { new_email: newEmail, password } });
}

export function patchMe(body: { name: string }): Promise<AuthUser> {
  return call('/auth/me', { method: 'PATCH', body });
}

export function setupTwoFactor(password: string): Promise<{ secret: string; otpauth_uri: string }> {
  return call('/auth/2fa/setup', { method: 'POST', body: { password } });
}

export function enableTwoFactor(code: string): Promise<{ enabled: boolean; recovery_codes: string[] }> {
  return call('/auth/2fa/enable', { method: 'POST', body: { code } });
}

export function disableTwoFactor(password: string, code: string): Promise<{ enabled: boolean }> {
  return call('/auth/2fa/disable', { method: 'POST', body: { password, code } });
}

export function regenerateRecoveryCodes(password: string): Promise<{ recovery_codes: string[] }> {
  return call('/auth/2fa/recovery-codes', { method: 'POST', body: { password } });
}

export async function fetchSessions(): Promise<SessionRow[]> {
  const r = await call<{ sessions?: SessionRow[] }>('/auth/sessions');
  return r.sessions ?? [];
}

export function revokeSession(id: string): Promise<{ revoked: boolean }> {
  return call(`/auth/sessions/${enc(id)}`, { method: 'DELETE' });
}

export function logoutAll(): Promise<unknown> {
  return call('/auth/logout-all', { method: 'POST', body: {} });
}

// ---------------------------------------------------------------------------
// Workspaces, members, invites
// ---------------------------------------------------------------------------

export async function fetchWorkspaces(all = false): Promise<{ workspaces: WorkspaceRow[]; current_id: string | null }> {
  const r = await call<{ workspaces?: WorkspaceRow[]; current_id?: string | null }>(`/workspaces${all ? '?all=1' : ''}`);
  return { workspaces: r.workspaces ?? [], current_id: r.current_id ?? null };
}

export function createWorkspace(body: { name: string; slug?: string }): Promise<WorkspaceRow> {
  return call('/workspaces', { method: 'POST', body });
}

export function fetchWorkspace(id: string): Promise<WorkspaceDetail> {
  return call(`/workspaces/${enc(id)}`);
}

export function patchWorkspace(id: string, body: { name?: string; slug?: string }): Promise<WorkspaceDetail> {
  return call(`/workspaces/${enc(id)}`, { method: 'PATCH', body });
}

export function deleteWorkspace(id: string, confirm: string): Promise<unknown> {
  return call(`/workspaces/${enc(id)}`, { method: 'DELETE', body: { confirm } });
}

export function transferWorkspace(id: string, userId: string): Promise<unknown> {
  return call(`/workspaces/${enc(id)}/transfer`, { method: 'POST', body: { user_id: userId } });
}

export function fetchUsageReport(id: string, history = false): Promise<UsageReport> {
  return call(`/workspaces/${enc(id)}/usage${history ? '?history=1' : ''}`);
}

export async function fetchMembers(id: string): Promise<MemberRow[]> {
  const r = await call<{ members?: MemberRow[] }>(`/workspaces/${enc(id)}/members`);
  return r.members ?? [];
}

export function patchMember(id: string, userId: string, role: MemberRole): Promise<unknown> {
  return call(`/workspaces/${enc(id)}/members/${enc(userId)}`, { method: 'PATCH', body: { role } });
}

export function removeMember(id: string, userId: string): Promise<unknown> {
  return call(`/workspaces/${enc(id)}/members/${enc(userId)}`, { method: 'DELETE' });
}

export async function fetchInvites(id: string): Promise<InviteRow[]> {
  const r = await call<{ invites?: InviteRow[] } | InviteRow[]>(`/workspaces/${enc(id)}/invites`);
  return Array.isArray(r) ? r : (r.invites ?? []);
}

export function createInvite(id: string, email: string, role: MemberRole): Promise<{ invite: InviteRow; delivery: Delivery; link?: string }> {
  return call(`/workspaces/${enc(id)}/invites`, { method: 'POST', body: { email, role } });
}

export function revokeInvite(id: string, inviteId: string): Promise<unknown> {
  return call(`/workspaces/${enc(id)}/invites/${enc(inviteId)}`, { method: 'DELETE' });
}

export function fetchInviteInfo(token: string): Promise<InviteInfo> {
  return call(`/invites/${enc(token)}`);
}

export function acceptInvite(token: string): Promise<{ workspace: { id: string; name: string } }> {
  return call(`/invites/${enc(token)}/accept`, { method: 'POST', body: {} });
}

export function fetchWorkspaceMcpPolicy(id: string): Promise<Omit<McpPolicy, 'http_enabled'>> {
  return call(`/workspaces/${enc(id)}/mcp-policy`);
}

export function putWorkspaceMcpPolicy(id: string, policy: Omit<McpPolicy, 'http_enabled'>): Promise<Omit<McpPolicy, 'http_enabled'>> {
  return call(`/workspaces/${enc(id)}/mcp-policy`, { method: 'PUT', body: { tools: policy.tools, per_token: policy.per_token } });
}

// ---------------------------------------------------------------------------
// Plans & billing
// ---------------------------------------------------------------------------

export async function fetchPlans(): Promise<Plan[]> {
  const r = await call<{ plans?: Plan[] }>('/plans');
  return r.plans ?? [];
}

export function fetchBilling(workspaceId: string): Promise<BillingInfo> {
  return call(`/billing${qs({ workspace_id: workspaceId })}`);
}

// ---------------------------------------------------------------------------
// Admin console
// ---------------------------------------------------------------------------

export function fetchAdminOverview(): Promise<AdminOverview> {
  return call('/admin/overview');
}

export function fetchAdminUsers(q: { q?: string; limit?: number; offset?: number } = {}): Promise<{ users: AdminUser[]; total: number }> {
  return call(`/admin/users${qs(q)}`);
}

export function createAdminUser(body: {
  email: string;
  name?: string;
  password?: string;
  role?: 'admin' | 'user';
  workspace_id?: string;
  workspace_role?: MemberRole;
}): Promise<{ user: AdminUser; delivery?: Delivery; link?: string }> {
  return call('/admin/users', { method: 'POST', body });
}

export function patchAdminUser(id: string, body: { name?: string; email?: string; role?: 'admin' | 'user' }): Promise<AdminUser> {
  return call(`/admin/users/${enc(id)}`, { method: 'PATCH', body });
}

export type AdminUserAction = 'reset-password' | 'logout' | 'disable' | 'enable' | 'verify-email' | 'reset-2fa';

export function adminUserAction(id: string, action: AdminUserAction): Promise<{ ok?: boolean; delivery?: Delivery; link?: string }> {
  return call(`/admin/users/${enc(id)}/${action}`, { method: 'POST', body: {} });
}

export function deleteAdminUser(id: string, transferTo?: string): Promise<unknown> {
  return call(`/admin/users/${enc(id)}${qs({ transfer_to: transferTo })}`, { method: 'DELETE' });
}

export function fetchAdminWorkspaces(
  q: { q?: string; status?: string; limit?: number; offset?: number } = {},
): Promise<{ workspaces: AdminWorkspace[]; total: number }> {
  return call(`/admin/workspaces${qs(q)}`);
}

export function fetchAdminWorkspace(id: string): Promise<AdminWorkspace & { members?: MemberRow[]; sites_list?: unknown }> {
  return call(`/admin/workspaces/${enc(id)}`);
}

export function patchAdminWorkspace(id: string, body: { name?: string; plan_id?: string; status?: string }): Promise<AdminWorkspace> {
  return call(`/admin/workspaces/${enc(id)}`, { method: 'PATCH', body });
}

export function adminWorkspaceAction(id: string, action: 'suspend' | 'resume' | 'restore'): Promise<unknown> {
  return call(`/admin/workspaces/${enc(id)}/${action}`, { method: 'POST', body: {} });
}

export function deleteAdminWorkspace(id: string): Promise<unknown> {
  return call(`/admin/workspaces/${enc(id)}`, { method: 'DELETE' });
}

export function createPlan(body: Omit<Plan, 'sort'> & { sort?: number }): Promise<Plan> {
  return call('/admin/plans', { method: 'POST', body });
}

export function patchPlan(id: string, body: Partial<Omit<Plan, 'id'>>): Promise<Plan> {
  return call(`/admin/plans/${enc(id)}`, { method: 'PATCH', body });
}

export function deletePlan(id: string): Promise<unknown> {
  return call(`/admin/plans/${enc(id)}`, { method: 'DELETE' });
}

export async function fetchAdminInvites(): Promise<InviteRow[]> {
  const r = await call<{ invites?: InviteRow[] } | InviteRow[]>('/admin/invites');
  return Array.isArray(r) ? r : (r.invites ?? []);
}

export function fetchAdminEmail(): Promise<{ configured: boolean; smtp: Partial<SmtpSettings>; outbox: OutboxRow[] }> {
  return call('/admin/email');
}

export function sendTestEmail(to: string): Promise<{ ok: boolean; delivery: Delivery; error?: string }> {
  return call('/admin/email/test', { method: 'POST', body: { to } });
}

export function deleteOutboxRow(id: string): Promise<unknown> {
  return call(`/admin/email/outbox/${enc(id)}`, { method: 'DELETE' });
}

export function fetchPlatform(): Promise<PlatformSettings> {
  return call('/admin/platform');
}

export function putPlatform(patch: PlatformPatch): Promise<PlatformSettings> {
  return call('/admin/platform', { method: 'PUT', body: patch });
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

export const ROLE_RANK: Record<string, number> = { viewer: 1, member: 2, admin: 3, owner: 4, platform_admin: 5 };

export function roleAtLeast(role: string | null | undefined, need: WorkspaceRole): boolean {
  return (ROLE_RANK[role ?? ''] ?? 0) >= (ROLE_RANK[need] ?? 99);
}

export const LIMIT_LABELS: Record<string, string> = {
  sites: 'sites',
  posts_per_site: 'posts per site',
  link_runs_per_month: 'link runs per month',
  ai_tokens_per_month: 'AI tokens per month',
  members: 'members',
  api_tokens: 'API tokens',
  schedules: 'schedules',
  auto_linker: 'Auto-Linker',
  mcp: 'MCP access',
};

export const NUMERIC_LIMITS = [
  'sites',
  'posts_per_site',
  'link_runs_per_month',
  'ai_tokens_per_month',
  'members',
  'api_tokens',
  'schedules',
] as const;

export type NumericLimit = (typeof NUMERIC_LIMITS)[number];

/** Usage value that a limit key is measured against. */
export function usageFor(key: NumericLimit, u: WorkspaceUsage): number {
  switch (key) {
    case 'sites':
      return u.sites;
    case 'posts_per_site':
      return u.posts_max_site;
    case 'link_runs_per_month':
      return u.link_runs_month;
    case 'ai_tokens_per_month':
      return u.ai_tokens_month;
    case 'members':
      return u.members + (u.pending_invites ?? 0);
    case 'api_tokens':
      return u.api_tokens;
    case 'schedules':
      return u.schedules;
  }
}
