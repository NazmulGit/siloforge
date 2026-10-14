// Typed helpers for the SiloForge v2 API (/api/v2/*). Same auth + error rules as
// api.ts (Bearer JWT or API token, JSON bodies, 401 → /login). A 404/503 from a
// v2 endpoint means "this server doesn't have the feature" (older v1 server, or
// the feature is switched off); callers check isUnavailable() and show a notice
// instead of an error.

import { ApiError, getToken, handleUnauthorized, requestAt, type RequestInitLite } from './api';
import type {
  ApiTokenCreated,
  ApiTokenRow,
  AuditRow,
  BackupKind,
  BackupRow,
  BackupVerifyResult,
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
  const token = getToken();
  let res: Response;
  try {
    res = await fetch(`${V2}/backups/${encodeURIComponent(id)}/download`, {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
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

export async function fetchTokens(): Promise<ApiTokenRow[]> {
  return listOf<ApiTokenRow>(await v2<unknown>('/tokens'), 'tokens');
}

export function createToken(body: { name: string; scopes: TokenScope[]; expires_in_days?: number }): Promise<ApiTokenCreated> {
  return v2<ApiTokenCreated>('/tokens', { method: 'POST', body });
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
