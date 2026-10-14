// Typed helpers for the SiloForge v2 API (/api/v2/*). Same auth + error rules as
// api.ts (Bearer JWT or API token, JSON bodies, 401 → /login). A 404/503 from a
// v2 endpoint means "this server doesn't have the feature" (older v1 server, or
// the feature is switched off); callers check isUnavailable() and show a notice
// instead of an error.

import { ApiError, authHeaders, handleUnauthorized, requestAt, type RequestInitLite } from './api';
import type {
  ApiTokenCreated,
  ApiTokenRow,
  AuditRow,
  BackupKind,
  BackupRow,
  BackupVerifyResult,
  DocMeta,
  LogRow,
  LogsPage,
  LogsQuery,
  LogStats,
  DocPage,
  DownloadItem,
  DownloadsList,
  McpPolicy,
  McpToolStat,
  ServerSettings,
  SettingsPatch,
  ChannelInput,
  ChannelTestResult,
  ErrorRow,
  JobLogRow,
  JobRunV2,
  JobsQuery,
  McpInfo,
  MetricSample,
  MetricsSeries,
  MetricsWindow,
  NotificationChannel,
  NotificationsList,
  NotificationRow,
  SystemInfo,
  TokenScope,
  LinkRun,
  LinkRunItem,
  LinkRunItemsQuery,
  LinkRunOptions,
  LinkRunPostGroup,
  LinkRunPostRef,
  Opportunities,
  AiTierView,
  AiUsage,
  AiUsageGroup,
  LinkRunEstimate,
} from './types2';

const V2 = '/api/v2';

/** Thrown when the server has no such v2 feature (404 / 503 / non-JSON SPA fallback). */
export class FeatureUnavailableError extends ApiError {
  constructor(status: number, message = 'This feature is not available on this server.') {
    super(status, message);
    this.name = 'FeatureUnavailableError';
  }
}

export function isUnavailable(e: unknown): boolean {
  return e instanceof FeatureUnavailableError;
}

/** Message for a caught error from a v2 call (friendly text for unavailable features). */
export function v2ErrorMessage(e: unknown, fallback = 'Request failed'): string {
  if (isUnavailable(e)) return 'Feature not available on this server.';
  return e instanceof Error ? e.message : fallback;
}

async function v2<T>(path: string, init: RequestInitLite = {}, opts: { item?: boolean } = {}): Promise<T> {
  try {
    return await requestAt<T>(V2, path, init);
  } catch (e) {
    // For item routes (/:id/...) a 404 is "not found", not "feature missing".
    if (e instanceof ApiError && (e.status === 503 || (e.status === 404 && !opts.item))) {
      throw new FeatureUnavailableError(e.status);
    }
    // A 2xx with a non-JSON body means a server that answered with its SPA page.
    if (e instanceof SyntaxError) throw new FeatureUnavailableError(200);
    throw e;
  }
}

/** Accept either a bare array or an object wrapping it under one of `keys`. */
function listOf<T>(res: unknown, ...keys: string[]): T[] {
  if (Array.isArray(res)) return res as T[];
  if (res && typeof res === 'object') {
    const obj = res as Record<string, unknown>;
    for (const k of [...keys, 'items', 'rows', 'data']) {
      if (Array.isArray(obj[k])) return obj[k] as T[];
    }
  }
  return [];
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

// ---- system ----

export function fetchSystem(): Promise<SystemInfo> {
  return v2<SystemInfo>('/system');
}

export async function fetchSystemMetrics(window: MetricsWindow): Promise<MetricsSeries> {
  const res = await v2<unknown>(`/system/metrics${qs({ window })}`);
  return { window, samples: listOf<MetricSample>(res, 'samples', 'metrics') };
}

export async function fetchErrors(): Promise<ErrorRow[]> {
  return listOf<ErrorRow>(await v2<unknown>('/errors'), 'errors');
}

export function resolveError(fingerprint: string): Promise<unknown> {
  return v2<unknown>(`/errors/${encodeURIComponent(fingerprint)}/resolve`, { method: 'POST', body: {} }, { item: true });
}

// ---- jobs ----

export async function fetchJobsV2(query: JobsQuery = {}): Promise<JobRunV2[]> {
  const res = await v2<unknown>(`/jobs${qs({ ...query })}`);
  return listOf<JobRunV2>(res, 'jobs');
}

export function cancelJob(jobId: string): Promise<unknown> {
  return v2<unknown>(`/jobs/${encodeURIComponent(jobId)}/cancel`, { method: 'POST', body: {} }, { item: true });
}

export async function fetchJobLogs(jobId: string, afterId?: number): Promise<JobLogRow[]> {
  const res = await v2<unknown>(`/jobs/${encodeURIComponent(jobId)}/logs${qs({ after_id: afterId })}`, {}, { item: true });
  return listOf<JobLogRow>(res, 'logs', 'lines');
}

// ---- notifications ----

export async function fetchNotifications(query: { unread?: boolean; limit?: number } = {}): Promise<NotificationsList> {
  const res = await v2<unknown>(`/notifications${qs({ unread: query.unread ? 1 : undefined, limit: query.limit })}`);
  const notifications = listOf<NotificationRow>(res, 'notifications');
  const unread =
    res && typeof res === 'object' && !Array.isArray(res) && typeof (res as { unread?: unknown }).unread === 'number'
      ? (res as { unread: number }).unread
      : undefined;
  return { notifications, unread };
}

export function markNotificationsRead(body: { ids?: number[]; all?: boolean }): Promise<unknown> {
  return v2<unknown>('/notifications/read', { method: 'POST', body });
}

export async function fetchChannels(): Promise<NotificationChannel[]> {
  return listOf<NotificationChannel>(await v2<unknown>('/notifications/channels'), 'channels');
}

export function createChannel(body: ChannelInput): Promise<NotificationChannel> {
  return v2<NotificationChannel>('/notifications/channels', { method: 'POST', body });
}

export function patchChannel(id: string, body: Partial<ChannelInput>): Promise<NotificationChannel> {
  return v2<NotificationChannel>(`/notifications/channels/${encodeURIComponent(id)}`, { method: 'PATCH', body }, { item: true });
}

export function deleteChannel(id: string): Promise<unknown> {
  return v2<unknown>(`/notifications/channels/${encodeURIComponent(id)}`, { method: 'DELETE' }, { item: true });
}

export function testChannel(id: string): Promise<ChannelTestResult> {
  return v2<ChannelTestResult>(`/notifications/channels/${encodeURIComponent(id)}/test`, { method: 'POST', body: {} }, { item: true });
}

// ---- backups ----

export async function fetchBackups(): Promise<BackupRow[]> {
  return listOf<BackupRow>(await v2<unknown>('/backups'), 'backups');
}

export function createBackup(body: {
  kind: BackupKind;
  passphrase?: string;
  note?: string;
  /** Skip encryption even when a server passphrase is configured (secrets.env is then left out). */
  unencrypted?: boolean;
}): Promise<unknown> {
  return v2<unknown>('/backups', { method: 'POST', body });
}

export function verifyBackup(id: string, passphrase?: string): Promise<BackupVerifyResult> {
  return v2<BackupVerifyResult>(
    `/backups/${encodeURIComponent(id)}/verify`,
    { method: 'POST', body: passphrase ? { passphrase } : {} },
    { item: true },
  );
}

export function deleteBackup(id: string): Promise<unknown> {
  return v2<unknown>(`/backups/${encodeURIComponent(id)}`, { method: 'DELETE' }, { item: true });
}

/** Download a backup archive with the auth header and hand it to the browser as a file. */
export async function downloadBackup(id: string, fallbackName: string): Promise<void> {
  let res: Response;
  try {
    res = await fetch(`${V2}/backups/${encodeURIComponent(id)}/download`, {
      headers: authHeaders(),
    });
  } catch {
    throw new ApiError(0, 'Network error — is the API reachable?');
  }
  if (res.status === 401) handleUnauthorized();
  if (res.status === 503) throw new FeatureUnavailableError(503);
  if (!res.ok) throw new ApiError(res.status, `Download failed (${res.status})`);
  const disposition = res.headers.get('Content-Disposition') ?? '';
  const match = /filename\*?=(?:UTF-8'')?"?([^";]+)"?/i.exec(disposition);
  const name = match?.[1] ? decodeURIComponent(match[1]) : fallbackName;
  saveBlob(await res.blob(), name);
}

export function saveBlob(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

// ---- API tokens ----

/** API tokens; v2.4 `workspace_id` narrows to one workspace's tokens. */
export async function fetchTokens(workspaceId?: string): Promise<ApiTokenRow[]> {
  return listOf<ApiTokenRow>(await v2<unknown>(`/tokens${qs({ workspace_id: workspaceId })}`), 'tokens');
}

export function createToken(body: { name: string; scopes: TokenScope[]; expires_in_days?: number; workspace_id?: string }): Promise<ApiTokenCreated> {
  return v2<ApiTokenCreated>('/tokens', { method: 'POST', body });
}

export function patchToken(
  id: string,
  body: { name?: string; scopes?: TokenScope[]; expires_in_days?: number | null },
): Promise<ApiTokenRow> {
  return v2<ApiTokenRow>(`/tokens/${encodeURIComponent(id)}`, { method: 'PATCH', body }, { item: true });
}

export function revokeToken(id: string): Promise<unknown> {
  return v2<unknown>(`/tokens/${encodeURIComponent(id)}`, { method: 'DELETE' }, { item: true });
}

// ---- MCP ----

export function fetchMcpInfo(): Promise<McpInfo> {
  return v2<McpInfo>('/mcp');
}

// ---- audit ----

export async function fetchAudit(limit = 100): Promise<AuditRow[]> {
  return listOf<AuditRow>(await v2<unknown>(`/audit${qs({ limit })}`), 'entries', 'audit');
}

// ---- v2.1: server settings ----

export function fetchServerSettings(): Promise<ServerSettings> {
  return v2<ServerSettings>('/settings');
}

export function putServerSettings(patch: SettingsPatch): Promise<ServerSettings> {
  return v2<ServerSettings>('/settings', { method: 'PUT', body: patch });
}

export function restartServer(): Promise<unknown> {
  return v2<unknown>('/system/restart', { method: 'POST', body: {} });
}

/** GET /health (outside /api). Resolves true when the server answers 2xx. */
export async function pingHealth(): Promise<boolean> {
  try {
    const res = await fetch('/health', { cache: 'no-store' });
    return res.ok;
  } catch {
    return false;
  }
}

// ---- v2.1: MCP management ----

export function fetchMcpPolicy(): Promise<McpPolicy> {
  return v2<McpPolicy>('/mcp/policy');
}

export function putMcpPolicy(policy: McpPolicy): Promise<McpPolicy> {
  return v2<McpPolicy>('/mcp/policy', { method: 'PUT', body: policy });
}

export async function fetchMcpStats(): Promise<McpToolStat[]> {
  return listOf<McpToolStat>(await v2<unknown>('/mcp/stats'), 'tools', 'stats');
}

// ---- v2.1: docs ----

export async function fetchDocs(): Promise<DocMeta[]> {
  return listOf<DocMeta>(await v2<unknown>('/docs'), 'docs');
}

export function fetchDoc(slug: string): Promise<DocPage> {
  return v2<DocPage>(`/docs/${encodeURIComponent(slug)}`, {}, { item: true });
}

// ---- v2.1: downloads ----

export async function fetchDownloads(): Promise<DownloadsList> {
  const res = await v2<unknown>('/downloads');
  const downloads = listOf<DownloadItem>(res, 'downloads');
  const current =
    res && typeof res === 'object' && !Array.isArray(res) ? (res as { current_version?: unknown }).current_version : undefined;
  return { downloads, current_version: typeof current === 'string' ? current : undefined };
}

/** Download a release file with the auth header (fetch + blob). */
export async function downloadRelease(file: string): Promise<void> {
  let res: Response;
  try {
    res = await fetch(`${V2}/downloads/${encodeURIComponent(file)}`, {
      headers: authHeaders(),
    });
  } catch {
    throw new ApiError(0, 'Network error — is the API reachable?');
  }
  if (res.status === 401) handleUnauthorized();
  if (res.status === 503) throw new FeatureUnavailableError(503);
  if (!res.ok) throw new ApiError(res.status, `Download failed (${res.status})`);
  saveBlob(await res.blob(), file);
}

// ---- v2.1: unified logs ----

/** `platform` = the v2.4 admin audit route (`/admin/audit`, same filters, every workspace). */
export async function fetchLogs(query: LogsQuery, platform = false): Promise<LogsPage> {
  const res = await v2<unknown>(`${platform ? '/admin/audit' : '/logs'}${qs({ ...query })}`);
  const logs = listOf<LogRow>(res, 'logs');
  const nb = res && typeof res === 'object' && !Array.isArray(res) ? (res as { next_before_id?: unknown }).next_before_id : null;
  return { logs, next_before_id: typeof nb === 'number' ? nb : null };
}

function countMap(v: unknown): Record<string, number> {
  const out: Record<string, number> = {};
  if (Array.isArray(v)) {
    for (const r of v) {
      if (r && typeof r === 'object') {
        const o = r as Record<string, unknown>;
        const k = o.key ?? o.name ?? o.source ?? o.status;
        const n = o.count ?? o.n;
        if (typeof k === 'string' && typeof n === 'number') out[k] = n;
      }
    }
  } else if (v && typeof v === 'object') {
    for (const [k, n] of Object.entries(v as Record<string, unknown>)) if (typeof n === 'number') out[k] = n;
  }
  return out;
}

export async function fetchLogStats(since?: string): Promise<LogStats> {
  const res = (await v2<unknown>(`/logs/stats${qs({ since })}`)) as Record<string, unknown> | null;
  const o = res ?? {};
  const ai = o.ai && typeof o.ai === 'object' ? (o.ai as Record<string, unknown>) : null;
  const n = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : 0);
  return {
    total: typeof o.total === 'number' ? o.total : undefined,
    by_source: countMap(o.by_source ?? o.sources),
    by_status: countMap(o.by_status ?? o.statuses),
    ai: ai
      ? { calls: n(ai.calls), tokens_in: n(ai.tokens_in), tokens_out: n(ai.tokens_out), cost_usd: n(ai.cost_usd), cache_hits: n(ai.cache_hits) }
      : null,
  };
}

export async function downloadLogsCsv(query: LogsQuery): Promise<void> {
  let res: Response;
  try {
    res = await fetch(`${V2}/logs/export${qs({ ...query, limit: undefined, before_id: undefined, after_id: undefined })}`, {
      headers: authHeaders(),
    });
  } catch {
    throw new ApiError(0, 'Network error — is the API reachable?');
  }
  if (res.status === 401) handleUnauthorized();
  if (res.status === 404 || res.status === 503) throw new FeatureUnavailableError(res.status);
  if (!res.ok) throw new ApiError(res.status, `Export failed (${res.status})`);
  saveBlob(await res.blob(), `siloforge-logs-${new Date().toISOString().slice(0, 10)}.csv`);
}

// ---- v2.2: Link Runs (docs/specs/v2.2-link-runs.md §3) ----

function numField(o: Record<string, unknown>, ...keys: string[]): number | undefined {
  for (const k of keys) if (typeof o[k] === 'number') return o[k] as number;
  return undefined;
}

/** Known opportunity counters in display order (other numeric keys are appended). */
const OPP_KEYS = ['orphans', 'under_linked', 'semantic_pairs', 'keyword_rule_hits', 'broken_internal', 'silo_gaps'];
const OPP_META = new Set([
  'posts_total',
  'posts',
  'total_posts',
  'last_crawl_at',
  'last_crawl_age_hours',
  'last_crawl_age_h',
  'last_crawl_age_s',
  'last_crawl_age_secs',
  'limit',
]);

export async function fetchOpportunities(siteId: string, limit?: number): Promise<Opportunities> {
  const res = await v2<unknown>(`/sites/${encodeURIComponent(siteId)}/opportunities${qs({ limit })}`);
  const o = res && typeof res === 'object' && !Array.isArray(res) ? (res as Record<string, unknown>) : {};
  const counts: Record<string, number> = { ...countMap(o.counts ?? o.by_type ?? o.opportunities) };
  for (const [k, v] of Object.entries(o)) {
    if (typeof v === 'number' && !OPP_META.has(k) && !(k in counts)) counts[k] = v;
  }
  const ordered = [
    ...OPP_KEYS.filter((k) => k in counts),
    ...Object.keys(counts)
      .filter((k) => !OPP_KEYS.includes(k))
      .sort(),
  ];
  const ageS = numField(o, 'last_crawl_age_s', 'last_crawl_age_secs');
  const ageH = numField(o, 'last_crawl_age_hours', 'last_crawl_age_h');
  const last = o.last_crawl_at;
  return {
    posts_total: numField(o, 'posts_total', 'posts', 'total_posts'),
    last_crawl_at: typeof last === 'string' ? last : null,
    last_crawl_age_hours: ageH ?? (ageS !== undefined ? ageS / 3600 : null),
    counts: ordered.map((key) => ({ key, count: counts[key] ?? 0 })),
    by_strategy: strategyCounts(o.by_strategy),
  };
}

/** The headline number per strategy key of `by_strategy`; the 2.2 server sends objects
 * (`semantic: {pairs, threshold, …}`), older mocks plain numbers. */
const STRATEGY_COUNT_FIELD: Record<string, string> = {
  semantic: 'pairs',
  keyword_rules: 'hits',
  silo: 'gaps',
  orphan_rescue: 'contextual_orphans',
  money_pages: 'money_pages',
  fix_broken: 'broken_links',
};
function strategyCounts(v: unknown): Record<string, number> {
  const out = countMap(v);
  if (v && typeof v === 'object' && !Array.isArray(v)) {
    for (const [k, field] of Object.entries(STRATEGY_COUNT_FIELD)) {
      const n = ((v as Record<string, unknown>)[k] as Record<string, unknown> | undefined)?.[field];
      if (typeof n === 'number' && !(k in out)) out[k] = n;
    }
  }
  return out;
}

function runOf(res: unknown): LinkRun {
  if (res && typeof res === 'object' && 'run' in res && (res as { run?: unknown }).run) {
    return (res as { run: LinkRun }).run;
  }
  return res as LinkRun;
}

export async function createLinkRun(siteId: string, options: LinkRunOptions, name?: string): Promise<LinkRun> {
  const res = await v2<unknown>(`/sites/${encodeURIComponent(siteId)}/link-runs`, {
    method: 'POST',
    body: name ? { name, options } : { options },
  });
  return runOf(res);
}

export async function fetchLinkRuns(siteId: string): Promise<LinkRun[]> {
  return listOf<LinkRun>(await v2<unknown>(`/sites/${encodeURIComponent(siteId)}/link-runs`), 'runs', 'link_runs');
}

export async function fetchLinkRun(runId: string): Promise<LinkRun> {
  return runOf(await v2<unknown>(`/link-runs/${encodeURIComponent(runId)}`, {}, { item: true }));
}

function postRef(v: unknown): LinkRunPostRef {
  const r = (v ?? {}) as Record<string, unknown>;
  return { id: String(r.id ?? ''), title: String(r.title ?? ''), url: String(r.url ?? '') };
}

function normItem(raw: unknown): LinkRunItem {
  const o = (raw ?? {}) as Record<string, unknown>;
  const alts = o.alternates ?? o.anchor_alternates;
  return {
    ...(o as unknown as LinkRunItem),
    edge_id: String(o.edge_id ?? o.id ?? ''),
    source: postRef(o.source),
    target: postRef(o.target),
    anchor: String(o.anchor ?? ''),
    status: String(o.status ?? 'suggested'),
    alternates: Array.isArray(alts)
      ? alts.map((a) => (typeof a === 'string' ? a : String((a as { anchor?: unknown } | null)?.anchor ?? ''))).filter(Boolean)
      : [],
  };
}

/** Items grouped by source post (`group=post`). A flat `{items}` answer is grouped client-side. */
export async function fetchLinkRunGroups(
  runId: string,
  query: LinkRunItemsQuery = {},
): Promise<{ posts: LinkRunPostGroup[]; total: number; totalPosts: number }> {
  const res = await v2<unknown>(
    `/link-runs/${encodeURIComponent(runId)}/items${qs({ ...query, group: 'post' })}`,
    {},
    { item: true },
  );
  const o = res && typeof res === 'object' && !Array.isArray(res) ? (res as Record<string, unknown>) : {};
  let posts: LinkRunPostGroup[];
  if (Array.isArray(o.posts)) {
    posts = (o.posts as Array<{ post?: unknown; links?: unknown[] } | null>).map((g) => {
      const links = (Array.isArray(g?.links) ? g.links : []).map(normItem);
      const p = postRef(g?.post);
      return {
        post: {
          id: p.id || (links[0]?.source.id ?? ''),
          title: p.title || (links[0]?.source.title ?? ''),
          url: p.url || (links[0]?.source.url ?? ''),
        },
        links,
      };
    });
  } else {
    const map = new Map<string, LinkRunPostGroup>();
    for (const it of listOf<unknown>(res, 'items').map(normItem)) {
      const g = map.get(it.source.id) ?? { post: it.source, links: [] };
      g.links.push(it);
      map.set(it.source.id, g);
    }
    posts = [...map.values()];
  }
  // group=post: `total` counts posts (the paging unit), `total_items` the links (2.2 §8)
  const items = posts.reduce((n, g) => n + g.links.length, 0);
  const total = typeof o.total_items === 'number' ? o.total_items : typeof o.total === 'number' && !Array.isArray(o.posts) ? o.total : items;
  const totalPosts = Array.isArray(o.posts) && typeof o.total === 'number' ? o.total : posts.length;
  return { posts, total, totalPosts };
}

export function selectLinkRunItems(
  runId: string,
  body: { edge_ids?: string[]; all?: boolean; deselect?: boolean },
): Promise<unknown> {
  return v2<unknown>(`/link-runs/${encodeURIComponent(runId)}/select`, { method: 'POST', body }, { item: true });
}

export async function patchLinkRunItem(
  runId: string,
  edgeId: string,
  body: { anchor?: string; insert_module?: string },
): Promise<LinkRunItem | null> {
  const res = await v2<unknown>(
    `/link-runs/${encodeURIComponent(runId)}/items/${encodeURIComponent(edgeId)}`,
    { method: 'PATCH', body },
    { item: true },
  );
  if (res && typeof res === 'object') {
    if ('edge_id' in res) return normItem(res);
    if ('item' in res) return normItem((res as { item: unknown }).item);
  }
  return null;
}

export type LinkRunAction = 'deploy' | 'verify' | 'repair' | 'undo' | 'cancel';

/** POST /link-runs/{id}/<action>. Returns the run when the server sends one back. */
export async function linkRunAction(runId: string, action: LinkRunAction): Promise<LinkRun | null> {
  const res = await v2<unknown>(`/link-runs/${encodeURIComponent(runId)}/${action}`, { method: 'POST', body: {} }, { item: true });
  const r = runOf(res) as unknown;
  if (r && typeof r === 'object' && typeof (r as { id?: unknown }).id === 'string' && 'status' in r) return r as LinkRun;
  return null;
}

// ---- v2.3: AI tiers, usage, Link Run estimate (docs/specs/v2.3-ai-tiers.md §6) ----

export async function fetchAiTiers(siteId?: string): Promise<AiTierView[]> {
  return listOf<AiTierView>(await v2<unknown>(`/ai/tiers${qs({ site_id: siteId })}`), 'tiers');
}

export async function fetchAiUsage(query: {
  since?: string;
  until?: string;
  group?: AiUsageGroup;
  site_id?: string;
  run_id?: string;
}): Promise<AiUsage> {
  const res = (await v2<unknown>(`/ai/usage${qs({ ...query })}`)) as Partial<AiUsage> | null;
  const r = res ?? {};
  return {
    group: typeof r.group === 'string' ? r.group : (query.group ?? 'day'),
    since: r.since,
    until: r.until,
    rows: Array.isArray(r.rows) ? r.rows : [],
    totals: r.totals ?? { key: 'total', calls: 0, tokens_in: 0, tokens_out: 0, cost_usd: 0, cache_hits: 0, errors: 0 },
    budget: r.budget ?? null,
  };
}

export function estimateLinkRun(siteId: string, options: LinkRunOptions): Promise<LinkRunEstimate> {
  return v2<LinkRunEstimate>(`/sites/${encodeURIComponent(siteId)}/link-runs/estimate`, {
    method: 'POST',
    body: { options },
  });
}
