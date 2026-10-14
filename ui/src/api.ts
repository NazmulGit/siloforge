// Fetch wrapper + typed endpoint helpers. All calls hit relative /api/v1/...
// (the siloforge binary serves API + UI on one origin; vite dev server proxies to :8600).
// v2 helpers live in api2.ts and share requestAt() below.

import type {
  AnchorReport,
  AnchorType,
  ClusterExport,
  ClusterImportResult,
  ClusterInfo,
  CsvImportResult,
  DashboardData,
  EdgeStatus,
  EdgeView,
  GraphReport,
  HistoryResponse,
  HistoryRow,
  IgnoreKeyword,
  JobRun,
  KeywordRule,
  LinkEdge,
  LinkHealthReport,
  LinkReportRow,
  MetricsReport,
  ModelChain,
  ModelChainRung,
  ModelChainWithRungs,
  ModelProvider,
  ModelRole,
  ModelRow,
  PostsResponse,
  ProbeResult,
  ProviderKind,
  ProvidersHealthReport,
  SavedSchedule,
  ScheduleKind,
  Site,
  SiteListItem,
  SiteMode,
  SiteSchedule,
  SiteSettings,
  SiloTopology,
} from './types';
import { emitEmailUnverified, emitPlanLimit, scopeHeaders, type PlanLimitDetail } from './wsStore';

const TOKEN_KEY = 'sf_token';

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token);
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
}

export class ApiError extends Error {
  readonly status: number;
  /** Machine-readable `error` field of the body, when the server sent one. */
  readonly code?: string;
  constructor(status: number, message: string, code?: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

/** HTTP 402 from a plan limit (v2.4 §9.2). The upgrade prompt is shown globally. */
export class PlanLimitError extends ApiError {
  readonly detail: PlanLimitDetail;
  constructor(message: string, detail: PlanLimitDetail) {
    super(402, message, 'plan limit');
    this.name = 'PlanLimitError';
    this.detail = detail;
  }
}

export function isPlanLimit(e: unknown): e is PlanLimitError {
  return e instanceof PlanLimitError;
}

export interface RequestInitLite {
  method?: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
  body?: unknown;
}

/** Shared 401 handling: drop the token and hard-redirect to the login page. */
export function handleUnauthorized(): never {
  clearToken();
  window.location.assign('/login');
  throw new ApiError(401, 'Session expired');
}

/** Bearer + workspace scope headers for every API call (also used by blob downloads). */
export function authHeaders(extra: Record<string, string> = {}): Record<string, string> {
  const headers: Record<string, string> = { ...scopeHeaders(), ...extra };
  const token = getToken();
  if (token) headers['Authorization'] = `Bearer ${token}`;
  return headers;
}

interface ErrorBody {
  message?: string;
  error?: string;
  key?: string;
  limit?: number | null;
  used?: number | null;
  plan?: string;
  workspace_id?: string;
}

async function errorBody(res: Response): Promise<ErrorBody> {
  try {
    const b = (await res.json()) as unknown;
    return b && typeof b === 'object' ? (b as ErrorBody) : {};
  } catch {
    return {}; /* non-JSON error body */
  }
}

/**
 * Turn a non-OK response into an ApiError. 402 plan limits raise the global upgrade prompt;
 * 403 "email not verified" raises the verify-email banner.
 */
async function toApiError(res: Response, fallback: string): Promise<ApiError> {
  const body = await errorBody(res);
  const msg = body.message ?? body.error ?? fallback;
  if (res.status === 402 && (body.error === 'plan limit' || body.key)) {
    const detail: PlanLimitDetail = {
      key: body.key ?? 'usage',
      limit: typeof body.limit === 'number' ? body.limit : null,
      used: typeof body.used === 'number' ? body.used : null,
      plan: body.plan ?? 'current',
      workspace_id: body.workspace_id,
      message: body.message,
    };
    emitPlanLimit(detail);
    return new PlanLimitError(msg, detail);
  }
  if (res.status === 403 && /email not verified/i.test(`${body.error ?? ''} ${body.message ?? ''}`)) emitEmailUnverified();
  return new ApiError(res.status, msg, body.error);
}

/** Auth routes where a 401 means "wrong credentials / password", not "session expired". */
const NO_REDIRECT_401 = new Set([
  '/auth/login',
  '/auth/register',
  '/auth/2fa/login',
  '/auth/forgot',
  '/auth/reset',
  '/auth/verify-email',
  '/auth/config',
  '/auth/2fa/setup',
  '/auth/2fa/disable',
  '/auth/2fa/recovery-codes',
  '/auth/change-email',
  '/auth/change-password',
]);

/** JSON request against an API base (`/api/v1` or `/api/v2`). */
export async function requestAt<T>(base: string, path: string, init: RequestInitLite = {}): Promise<T> {
  const headers = authHeaders({ 'Content-Type': 'application/json' });

  let res: Response;
  try {
    res = await fetch(`${base}${path}`, {
      method: init.method ?? 'GET',
      headers,
      body: init.body !== undefined ? JSON.stringify(init.body) : undefined,
    });
  } catch {
    throw new ApiError(0, 'Network error — is the API reachable?');
  }

  const isAuthCall = NO_REDIRECT_401.has(path.split('?')[0] ?? path) || path.startsWith('/invites/');
  if (res.status === 401 && !isAuthCall) handleUnauthorized();

  if (!res.ok) throw await toApiError(res, `Request failed (${res.status})`);

  return (await res.json()) as T;
}

function request<T>(path: string, init: RequestInitLite = {}): Promise<T> {
  return requestAt<T>('/api/v1', path, init);
}

/** GET a binary/CSV resource with the Bearer header; 401 redirects to /login like JSON calls. */
export async function fetchBlob(url: string, failLabel = 'Download failed'): Promise<Blob> {
  let res: Response;
  try {
    res = await fetch(url, { headers: authHeaders() });
  } catch {
    throw new ApiError(0, 'Network error — is the API reachable?');
  }
  if (res.status === 401) handleUnauthorized();
  if (!res.ok) throw await toApiError(res, `${failLabel} (${res.status})`);
  return res.blob();
}

// ---- auth ----
export interface AuthUser {
  id: string;
  email: string;
  name: string;
  role: 'admin' | 'user';
  /** v2.4 /api/v2/auth/me extras (absent on older servers). */
  email_verified?: boolean;
  email_verified_at?: string | null;
  pending_email?: string | null;
  two_factor_enabled?: boolean;
  recovery_codes_left?: number;
  platform_admin?: boolean;
  created_at?: string;
  default_workspace_id?: string | null;
  workspaces?: Array<{ id: string; name: string; slug: string; role: string; status: string; plan?: { id: string; name: string } }>;
}

export function isPlatformAdmin(u: AuthUser | null | undefined): boolean {
  return !!u && (u.platform_admin === true || u.role === 'admin');
}

export interface AuthResponse {
  token: string;
  user: AuthUser;
  session_id?: string;
  expires_at?: string;
}

export type LoginResult = AuthResponse | { two_factor_required: true; challenge: string };

export function needsTwoFactor(r: LoginResult): r is { two_factor_required: true; challenge: string } {
  return (r as { two_factor_required?: boolean }).two_factor_required === true;
}

/** v1 login (kept for older servers and scripts). */
export function login(email: string, password: string): Promise<AuthResponse> {
  return request<AuthResponse>('/auth/login', { method: 'POST', body: { email, password } });
}

function v2Missing(e: unknown): boolean {
  return (e instanceof ApiError && (e.status === 404 || e.status === 405)) || e instanceof SyntaxError;
}

/** v2.4 login (remember me + 2FA); falls back to v1 when the server has no v2 auth. */
export async function loginV2(email: string, password: string, remember: boolean): Promise<LoginResult> {
  try {
    return await requestAt<LoginResult>('/api/v2', '/auth/login', { method: 'POST', body: { email, password, remember } });
  } catch (e) {
    if (v2Missing(e)) return login(email, password);
    throw e;
  }
}

export function loginTwoFactor(challenge: string, code: string): Promise<AuthResponse> {
  return requestAt<AuthResponse>('/api/v2', '/auth/2fa/login', { method: 'POST', body: { challenge, code } });
}

/** Revoke the current session server-side; never throws and never redirects. */
export async function logoutSession(): Promise<void> {
  if (!getToken()) return;
  try {
    await fetch('/api/v2/auth/logout', { method: 'POST', headers: authHeaders({ 'Content-Type': 'application/json' }), body: '{}' });
  } catch {
    /* offline — the token is dropped locally anyway */
  }
}

export function register(email: string, password: string, name?: string): Promise<AuthResponse> {
  return request<AuthResponse>('/auth/register', {
    method: 'POST',
    body: name ? { email, password, name } : { email, password },
  });
}

/** v2.4 /auth/me (workspaces, 2FA, verification), falling back to the v1 shape. */
export async function fetchMe(): Promise<AuthUser> {
  try {
    return await requestAt<AuthUser>('/api/v2', '/auth/me');
  } catch (e) {
    if (v2Missing(e)) return request<AuthUser>('/auth/me');
    throw e;
  }
}

export async function changePassword(currentPassword: string, newPassword: string): Promise<{ ok?: boolean }> {
  const res = await request<{ ok?: boolean; token?: string }>('/auth/change-password', {
    method: 'POST',
    body: { current_password: currentPassword, new_password: newPassword },
  });
  // v2 revokes older sessions on password change and returns a fresh token for this one.
  if (res.token) setToken(res.token);
  return res;
}

// ---- Search Console (GSC) — per-site connector ----
export interface GscStatus {
  connected: boolean;
  client_email: string | null;
  saved_at: string | null;
}

export function fetchGscStatus(siteId: string): Promise<GscStatus> {
  return request<GscStatus>(`/sites/${siteId}/gsc`);
}

export function saveGsc(siteId: string, credentialsJson: string): Promise<GscStatus> {
  return request<GscStatus>(`/sites/${siteId}/gsc`, {
    method: 'POST',
    body: { credentials_json: credentialsJson },
  });
}

export function disconnectGsc(siteId: string): Promise<{ connected: boolean }> {
  return request<{ connected: boolean }>(`/sites/${siteId}/gsc`, { method: 'DELETE' });
}

// ---- usage / plan ----
export interface UsageSummary {
  sites: number;
  posts: number;
  paragraphs: number;
  suggested: number;
  approved: number;
  applied: number;
  clusters: number;
}

export function fetchUsage(): Promise<UsageSummary> {
  return request<UsageSummary>('/usage');
}

// ---- sites ----
export function fetchSites(): Promise<SiteListItem[]> {
  return request<SiteListItem[]>('/sites');
}

export function createSite(body: { domain: string; wp_url?: string; mode?: SiteMode }): Promise<Site> {
  return request<Site>('/sites', { method: 'POST', body });
}

export function triggerCrawl(siteId: string): Promise<{ job_run_id: string }> {
  return request<{ job_run_id: string }>(`/sites/${siteId}/crawl`, { method: 'POST', body: {} });
}

/** `aiRefine` (v2, optional) overrides the site's `ai_anchor_refine` for this run only. */
export function triggerPlan(
  siteId: string,
  topology?: SiloTopology,
  opts: { aiRefine?: boolean } = {},
): Promise<{ job_run_id: string }> {
  return request<{ job_run_id: string }>(`/sites/${siteId}/plan`, {
    method: 'POST',
    body: {
      ...(topology ? { topology } : {}),
      ...(opts.aiRefine !== undefined ? { ai_refine: opts.aiRefine } : {}),
    },
  });
}

/** Select value for the per-run AI anchor option: '' = site setting. */
export type AiRefineChoice = '' | 'on' | 'off';

export function aiRefineOpt(c: AiRefineChoice): { aiRefine?: boolean } {
  return c === '' ? {} : { aiRefine: c === 'on' };
}

// ---- Archive/taxonomy Silo (Gap A) ----
export function triggerArchivePlan(siteId: string, topology?: SiloTopology): Promise<{ job_run_id: string }> {
  return request<{ job_run_id: string }>(`/sites/${siteId}/archive-plan`, {
    method: 'POST',
    body: topology ? { topology } : {},
  });
}

// ---- dashboard / reports / jobs ----
export function fetchDashboard(siteId: string): Promise<DashboardData> {
  return request<DashboardData>(`/dashboard/${siteId}`);
}

/** Per-post link report. The server has no duplicates filter (v1 ignored `?duplicates=1`, A9):
 *  filter on `duplicate_count > 0` client-side. */
export function fetchLinksReport(siteId: string): Promise<LinkReportRow[]> {
  return request<LinkReportRow[]>(`/reports/links/${siteId}`);
}

// ---- link history (Gap J) ----
export function fetchHistory(siteId: string): Promise<HistoryRow[]>;
export function fetchHistory(siteId: string, limit: number, offset?: number): Promise<HistoryResponse>;
export function fetchHistory(
  siteId: string,
  limit?: number,
  offset = 0,
): Promise<HistoryRow[] | HistoryResponse> {
  const pageLimit = limit ?? 50;
  const params = new URLSearchParams({ limit: String(pageLimit), offset: String(offset) });
  const response = request<HistoryResponse>(`/history/${siteId}?${params.toString()}`);
  // Preserve the original one-argument helper for compact consumers such as
  // Settings > Activity while the dedicated History page uses page metadata.
  return limit === undefined ? response.then((page) => page.rows) : response;
}

export function downloadHistoryCsv(siteId: string): Promise<Blob> {
  return fetchBlob(`/api/v1/history/${siteId}?format=csv`);
}

export function fetchGraphReport(
  siteId: string,
  limits: { maxNodes?: number; maxEdges?: number } = {},
): Promise<GraphReport> {
  const params = new URLSearchParams();
  if (limits.maxNodes !== undefined) params.set('max_nodes', String(limits.maxNodes));
  if (limits.maxEdges !== undefined) params.set('max_edges', String(limits.maxEdges));
  const qs = params.toString();
  return request<GraphReport>(`/reports/graph/${siteId}${qs ? `?${qs}` : ''}`);
}

export function fetchJobs(siteId: string): Promise<JobRun[]> {
  return request<JobRun[]>(`/jobs/${siteId}`);
}

/** One job run by id (v2 server adds this to /api/v1; 404 on older servers). */
export function fetchJobRun(jobRunId: string): Promise<JobRun> {
  return request<JobRun>(`/jobs/run/${jobRunId}`);
}

// ---- edges ----
export interface EdgeQuery {
  status?: EdgeStatus;
  tool?: string;
  limit?: number;
  offset?: number;
}

export function fetchEdges(siteId: string, query: EdgeQuery = {}): Promise<{ edges: EdgeView[]; total: number }> {
  const params = new URLSearchParams();
  if (query.status) params.set('status', query.status);
  if (query.tool) params.set('tool', query.tool);
  if (query.limit !== undefined) params.set('limit', String(query.limit));
  if (query.offset !== undefined) params.set('offset', String(query.offset));
  const qs = params.toString();
  return request<{ edges: EdgeView[]; total: number }>(`/edges/${siteId}${qs ? `?${qs}` : ''}`);
}

/** v1 routes cap a page at 1000; 500 keeps responses small. */
export const EDGE_PAGE_SIZE = 500;

/**
 * Fetch every edge matching the query by walking limit/offset pages until `total`
 * is reached (or `maxRows`, a safety cap for very large sites).
 */
export async function fetchAllEdges(
  siteId: string,
  query: Omit<EdgeQuery, 'limit' | 'offset'> = {},
  maxRows = 20000,
): Promise<{ edges: EdgeView[]; total: number }> {
  const all: EdgeView[] = [];
  let total = 0;
  for (let offset = 0; offset < maxRows; offset += EDGE_PAGE_SIZE) {
    const page = await fetchEdges(siteId, { ...query, limit: EDGE_PAGE_SIZE, offset });
    total = page.total;
    all.push(...page.edges);
    if (page.edges.length < EDGE_PAGE_SIZE || all.length >= total) break;
  }
  return { edges: all, total };
}

// ---- posts (Bulk Wizard — source content fetch) ----
export interface PostsQuery {
  search?: string;
  post_type?: string;
  cluster_id?: string;
  is_orphan?: boolean;
  limit?: number;
  offset?: number;
}

export function fetchPosts(siteId: string, query: PostsQuery = {}): Promise<PostsResponse> {
  const params = new URLSearchParams();
  if (query.search) params.set('search', query.search);
  if (query.post_type) params.set('post_type', query.post_type);
  if (query.cluster_id) params.set('cluster_id', query.cluster_id);
  if (query.is_orphan !== undefined) params.set('is_orphan', query.is_orphan ? '1' : '0');
  if (query.limit !== undefined) params.set('limit', String(query.limit));
  if (query.offset !== undefined) params.set('offset', String(query.offset));
  const qs = params.toString();
  return request<PostsResponse>(`/posts/${siteId}${qs ? `?${qs}` : ''}`);
}

export function decideEdge(edgeId: string, decision: 'approved' | 'rejected'): Promise<LinkEdge> {
  return request<LinkEdge>(`/edges/${edgeId}/decide`, { method: 'POST', body: { decision } });
}

export function editEdgeAnchor(edgeId: string, anchor: string, anchorType?: AnchorType): Promise<LinkEdge> {
  return request<LinkEdge>(`/edges/${edgeId}/anchor`, {
    method: 'PATCH',
    body: anchorType ? { anchor, anchor_type: anchorType } : { anchor },
  });
}

// ---- site settings (bulk single-mode / preset) ----
export function patchSiteSettings(siteId: string, settings: SiteSettings): Promise<Site> {
  return request<Site>(`/sites/${siteId}`, { method: 'PATCH', body: { settings } });
}

export function fetchSite(siteId: string): Promise<Site> {
  return request<Site>(`/sites/${siteId}`);
}

// ---- anchor report ----
export function fetchAnchorReport(siteId: string): Promise<AnchorReport> {
  return request<AnchorReport>(`/reports/anchors/${siteId}`);
}

// ---- rules: keyword linker ----
export function fetchKeywordRules(siteId: string): Promise<KeywordRule[]> {
  return request<KeywordRule[]>(`/rules/keywords/${siteId}`);
}

export function createKeywordRule(
  siteId: string,
  body: { keyword: string; target_url: string; anchor_type?: AnchorType; max_links?: number },
): Promise<KeywordRule> {
  return request<KeywordRule>(`/rules/keywords/${siteId}`, { method: 'POST', body });
}

export function patchKeywordRule(
  ruleId: string,
  body: { active?: boolean; max_links?: number; anchor_type?: AnchorType },
): Promise<KeywordRule> {
  return request<KeywordRule>(`/rules/keywords/${ruleId}`, { method: 'PATCH', body });
}

export function deleteKeywordRule(ruleId: string): Promise<{ deleted: boolean }> {
  return request<{ deleted: boolean }>(`/rules/keywords/${ruleId}`, { method: 'DELETE' });
}

export function runKeywordLinker(siteId: string): Promise<{ job_run_id: string }> {
  return request<{ job_run_id: string }>(`/rules/keywords/${siteId}/run`, { method: 'POST', body: {} });
}

// ---- rules: ignore keywords ----
export function fetchIgnoreKeywords(siteId: string): Promise<IgnoreKeyword[]> {
  return request<IgnoreKeyword[]>(`/rules/ignore/${siteId}`);
}

export function createIgnoreKeyword(siteId: string, phrase: string): Promise<IgnoreKeyword> {
  return request<IgnoreKeyword>(`/rules/ignore/${siteId}`, { method: 'POST', body: { phrase } });
}

export function deleteIgnoreKeyword(phraseId: string): Promise<{ deleted: boolean }> {
  return request<{ deleted: boolean }>(`/rules/ignore/${phraseId}`, { method: 'DELETE' });
}

// ---- custom silo network (Gap B) ----
export function fetchClusters(siteId: string): Promise<ClusterInfo[]> {
  return request<ClusterInfo[]>(`/silo/${siteId}/clusters`);
}

export function exportClusters(siteId: string): Promise<ClusterExport> {
  return request<ClusterExport>(`/silo/${siteId}/clusters/export`);
}

export function importClusters(siteId: string, data: ClusterExport): Promise<ClusterImportResult> {
  return request<ClusterImportResult>(`/silo/${siteId}/clusters/import`, { method: 'POST', body: data });
}

// ---- CSV connector (Gap K-CSV) ----
export async function importCsv(siteId: string, csvText: string): Promise<CsvImportResult> {
  let res: Response;
  try {
    res = await fetch(`/api/v1/csv/import/${siteId}`, {
      method: 'POST',
      headers: authHeaders({ 'Content-Type': 'text/csv' }),
      body: csvText,
    });
  } catch {
    throw new ApiError(0, 'Network error — is the API reachable?');
  }
  if (res.status === 401) handleUnauthorized();
  if (!res.ok) throw await toApiError(res, `Import failed (${res.status})`);
  return (await res.json()) as CsvImportResult;
}

export function downloadEdgesCsv(siteId: string): Promise<Blob> {
  return fetchBlob(`/api/v1/csv/export/${siteId}`);
}

// ---- bulk ----
export function bulkDecide(edgeIds: string[], decision: 'approved' | 'rejected'): Promise<{ updated: number }> {
  return request<{ updated: number }>('/bulk/decide', {
    method: 'POST',
    body: { edge_ids: edgeIds, decision },
  });
}

export function bulkRun(siteId: string, edgeIds?: string[]): Promise<{ job_run_id: string; count: number }> {
  return request<{ job_run_id: string; count: number }>('/bulk/run', {
    method: 'POST',
    body: edgeIds ? { site_id: siteId, edge_ids: edgeIds } : { site_id: siteId },
  });
}

export function bulkUndo(siteId: string, edgeIds?: string[]): Promise<{ job_run_id: string; count: number }> {
  return request<{ job_run_id: string; count: number }>('/bulk/undo', {
    method: 'POST',
    body: edgeIds ? { site_id: siteId, edge_ids: edgeIds } : { site_id: siteId },
  });
}

// ---- link health (checklist E) ----

/** Enqueue a crawl of every published link for the site. */
export function runLinkHealth(siteId: string, opts: { internal_only?: boolean } = {}): Promise<{ job_run_id: string }> {
  return request<{ job_run_id: string }>(`/sites/${siteId}/link-health`, {
    method: 'POST',
    body: opts.internal_only ? { internal_only: true } : {},
  });
}

export function fetchLinkHealth(
  siteId: string,
  query: { limit?: number; offset?: number } = {},
): Promise<LinkHealthReport> {
  const params = new URLSearchParams();
  if (query.limit !== undefined) params.set('limit', String(query.limit));
  if (query.offset !== undefined) params.set('offset', String(query.offset));
  const qs = params.toString();
  return request<LinkHealthReport>(`/reports/link-health/${siteId}${qs ? `?${qs}` : ''}`);
}

export function downloadLinkHealthCsv(siteId: string): Promise<Blob> {
  return fetchBlob(`/api/v1/reports/link-health/${siteId}?format=csv`);
}

// ---- schedules + metric trends (checklist F) ----

export function fetchSchedules(siteId: string): Promise<SiteSchedule[]> {
  return request<SiteSchedule[]>(`/schedules/${siteId}`);
}

export interface ScheduleInput {
  job_kind: ScheduleKind;
  cron: string;
  tz: string;
  active?: boolean;
  payload?: Record<string, unknown>;
}

export function saveSchedule(siteId: string, body: ScheduleInput): Promise<SavedSchedule> {
  return request<SavedSchedule>(`/schedules/${siteId}`, { method: 'POST', body });
}

export function deleteSchedule(scheduleId: string): Promise<{ deleted: boolean }> {
  return request<{ deleted: boolean }>(`/schedules/${scheduleId}`, { method: 'DELETE' });
}

export function runScheduleNow(
  siteId: string,
  body: { topology?: SiloTopology; skip_crawl?: boolean } = {},
): Promise<{ job_run_id: string }> {
  return request<{ job_run_id: string }>(`/schedules/${siteId}/run-now`, { method: 'POST', body });
}

export function fetchMetricsReport(siteId: string, trend?: number): Promise<MetricsReport> {
  return request<MetricsReport>(
    `/reports/metrics/${siteId}${trend !== undefined ? `?trend=${trend}` : ''}`,
  );
}

// ---- model providers / models / chains (ADMIN ONLY — 403 for everyone else) ----

export interface ProviderInput {
  name: string;
  label?: string;
  kind: ProviderKind;
  base_url: string;
  /** omit to leave the stored key untouched; '' clears it */
  api_key?: string;
  headers?: Record<string, string>;
  enabled?: boolean;
}

export function fetchProviders(): Promise<ModelProvider[]> {
  return request<ModelProvider[]>('/providers');
}

export function fetchProvidersHealth(): Promise<ProvidersHealthReport> {
  return request<ProvidersHealthReport>('/providers/health');
}

export function createProvider(body: ProviderInput): Promise<ModelProvider> {
  return request<ModelProvider>('/providers', { method: 'POST', body });
}

export function patchProvider(id: string, body: Partial<ProviderInput>): Promise<ModelProvider> {
  return request<ModelProvider>(`/providers/${id}`, { method: 'PATCH', body });
}

export function deleteProvider(id: string): Promise<{ deleted: boolean }> {
  return request<{ deleted: boolean }>(`/providers/${id}`, { method: 'DELETE' });
}

export function probeProvider(id: string): Promise<ProbeResult> {
  return request<ProbeResult>(`/providers/${id}/probe`, { method: 'POST', body: {} });
}

export interface ModelInput {
  provider_id: string;
  model_id: string;
  label?: string;
  role: ModelRole;
  context_tokens?: number;
  dim?: number;
  enabled?: boolean;
  price_in?: number;
  price_out?: number;
  notes?: string;
}

export function fetchModels(query: { provider_id?: string; role?: ModelRole } = {}): Promise<ModelRow[]> {
  const params = new URLSearchParams();
  if (query.provider_id) params.set('provider_id', query.provider_id);
  if (query.role) params.set('role', query.role);
  const qs = params.toString();
  return request<ModelRow[]>(`/models${qs ? `?${qs}` : ''}`);
}

export function createModel(body: ModelInput): Promise<ModelRow> {
  return request<ModelRow>('/models', { method: 'POST', body });
}

export function patchModel(id: string, body: Partial<Omit<ModelInput, 'provider_id' | 'role'>>): Promise<ModelRow> {
  return request<ModelRow>(`/models/${id}`, { method: 'PATCH', body });
}

export function deleteModel(id: string): Promise<{ deleted: boolean }> {
  return request<{ deleted: boolean }>(`/models/${id}`, { method: 'DELETE' });
}

export function probeModel(id: string): Promise<ProbeResult> {
  return request<ProbeResult>(`/models/${id}/probe`, { method: 'POST', body: {} });
}

export function fetchChains(query: { role?: ModelRole; site_id?: string; tier?: 'low' | 'main' | 'high' } = {}): Promise<ModelChainWithRungs[]> {
  const params = new URLSearchParams();
  if (query.role) params.set('role', query.role);
  if (query.site_id) params.set('site_id', query.site_id);
  if (query.tier) params.set('tier', query.tier);
  const qs = params.toString();
  return request<ModelChainWithRungs[]>(`/model-chains${qs ? `?${qs}` : ''}`);
}

export function createChain(body: {
  name: string;
  role: ModelRole;
  site_id?: string | null;
  active?: boolean;
  notes?: string;
  /** v2.3 AI tier (default main). */
  tier?: 'low' | 'main' | 'high';
}): Promise<ModelChain> {
  return request<ModelChain>('/model-chains', { method: 'POST', body });
}

export function patchChain(id: string, body: { name?: string; notes?: string; tier?: 'low' | 'main' | 'high' }): Promise<ModelChain> {
  return request<ModelChain>(`/model-chains/${id}`, { method: 'PATCH', body });
}

export function deleteChain(id: string): Promise<{ deleted: boolean }> {
  return request<{ deleted: boolean }>(`/model-chains/${id}`, { method: 'DELETE' });
}

export function setChainActive(id: string, active: boolean): Promise<ModelChain> {
  return request<ModelChain>(`/model-chains/${id}/${active ? 'activate' : 'deactivate'}`, {
    method: 'POST',
    body: {},
  });
}

export interface RungInput {
  model_row_id: string;
  timeout_ms?: number;
  max_retries?: number;
  enabled?: boolean;
}

/** Replaces the chain's rungs wholesale — array order IS the fallback order. */
export function putChainRungs(chainId: string, rungs: RungInput[]): Promise<ModelChainRung[]> {
  return request<ModelChainRung[]>(`/model-chains/${chainId}/rungs`, { method: 'PUT', body: { rungs } });
}

export function addChainRung(chainId: string, body: RungInput): Promise<ModelChainRung> {
  return request<ModelChainRung>(`/model-chains/${chainId}/rungs`, { method: 'POST', body });
}

/** `rungIds` must list every rung of the chain exactly once, in the new order. */
export function reorderChainRungs(chainId: string, rungIds: string[]): Promise<ModelChainRung[]> {
  return request<ModelChainRung[]>(`/model-chains/${chainId}/rungs/reorder`, {
    method: 'POST',
    body: { rung_ids: rungIds },
  });
}

export function deleteChainRung(rungId: string): Promise<{ deleted: boolean }> {
  return request<{ deleted: boolean }>(`/model-chain-rungs/${rungId}`, { method: 'DELETE' });
}
