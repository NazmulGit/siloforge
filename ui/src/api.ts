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
  constructor(status: number, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
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

function authHeaders(extra: Record<string, string> = {}): Record<string, string> {
  const headers: Record<string, string> = { ...extra };
  const token = getToken();
  if (token) headers['Authorization'] = `Bearer ${token}`;
  return headers;
}

async function errorMessage(res: Response, fallback: string): Promise<string> {
  try {
    const body = (await res.json()) as { message?: string; error?: string };
    return body.message ?? body.error ?? fallback;
  } catch {
    return fallback; /* non-JSON error body */
  }
}

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

  const isAuthCall = base === '/api/v1' && (path === '/auth/login' || path === '/auth/register');
  if (res.status === 401 && !isAuthCall) handleUnauthorized();

  if (!res.ok) throw new ApiError(res.status, await errorMessage(res, `Request failed (${res.status})`));

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
  if (!res.ok) throw new ApiError(res.status, await errorMessage(res, `${failLabel} (${res.status})`));
  return res.blob();
}

// ---- auth ----
export interface AuthUser {
  id: string;
  email: string;
  name: string;
  role: 'admin' | 'user';
}

export interface AuthResponse {
  token: string;
  user: AuthUser;
}

export function login(email: string, password: string): Promise<AuthResponse> {
  return request<AuthResponse>('/auth/login', { method: 'POST', body: { email, password } });
}

export function register(email: string, password: string, name?: string): Promise<AuthResponse> {
  return request<AuthResponse>('/auth/register', {
    method: 'POST',
    body: name ? { email, password, name } : { email, password },
  });
}

export function fetchMe(): Promise<AuthUser> {
  return request<AuthUser>('/auth/me');
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

export function triggerPlan(siteId: string, topology?: SiloTopology): Promise<{ job_run_id: string }> {
  return request<{ job_run_id: string }>(`/sites/${siteId}/plan`, {
    method: 'POST',
    body: topology ? { topology } : {},
  });
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

export function fetchLinksReport(siteId: string, duplicates = false): Promise<LinkReportRow[]> {
  return request<LinkReportRow[]>(`/reports/links/${siteId}${duplicates ? '?duplicates=1' : ''}`);
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
  if (!res.ok) throw new ApiError(res.status, await errorMessage(res, `Import failed (${res.status})`));
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
export function runLinkHealth(siteId: string): Promise<{ job_run_id: string }> {
  return request<{ job_run_id: string }>(`/sites/${siteId}/link-health`, { method: 'POST', body: {} });
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

export function fetchChains(query: { role?: ModelRole; site_id?: string } = {}): Promise<ModelChainWithRungs[]> {
  const params = new URLSearchParams();
  if (query.role) params.set('role', query.role);
  if (query.site_id) params.set('site_id', query.site_id);
  const qs = params.toString();
  return request<ModelChainWithRungs[]>(`/model-chains${qs ? `?${qs}` : ''}`);
}

export function createChain(body: {
  name: string;
  role: ModelRole;
  site_id?: string | null;
  active?: boolean;
  notes?: string;
}): Promise<ModelChain> {
  return request<ModelChain>('/model-chains', { method: 'POST', body });
}

export function patchChain(id: string, body: { name?: string; notes?: string }): Promise<ModelChain> {
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
