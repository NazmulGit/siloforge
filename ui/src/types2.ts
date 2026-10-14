// Wire types for the SiloForge v2 API (/api/v2/*). These mirror the Rust server's
// JSON. Optional fields are ones the UI can live without, so an older or partial
// server still renders. v1 shapes stay in types.ts.

import type { JobRun } from './types';

// ---- system / monitor ----

/** One resource sample (row of `system_metrics`, or the live sample). */
export interface MetricSample {
  ts: string;
  cpu_pct: number;
  proc_cpu_pct: number;
  mem_used_mb: number;
  mem_total_mb: number;
  proc_rss_mb: number;
  jobs_running?: number;
  jobs_queued?: number;
  disk_free_mb?: number | null;
}

/** sf-ops `Capacity`: static machine facts plus configured budgets. */
export interface SystemCapacity {
  physical_cores?: number;
  logical_cpus?: number;
  /** resources.cpu_threads resolved (work pool size). */
  cpu_threads?: number;
  max_concurrent_jobs?: number;
  mem_total_mb?: number;
  mem_available_mb?: number;
  vector_cache_mb?: number;
  recommended_max_concurrent_jobs?: number;
  gpu?: string | null;
  os?: string | null;
  hostname?: string | null;
}

/** GET /api/v2/system. Flat fields and `capacity` are both accepted. */
export interface SystemInfo {
  version: string;
  uptime_s?: number;
  uptime_secs?: number;
  capacity?: SystemCapacity | null;
  started_at?: string | null;
  hostname?: string | null;
  os?: string | null;
  home?: string | null;
  /** Logical CPU count of the machine. */
  cpu_threads?: number | null;
  /** Threads in the shared work pool (resources.cpu_threads). */
  pool_threads?: number | null;
  mem_total_mb?: number | null;
  disk_free_mb?: number | null;
  db_size_bytes?: number | null;
  jobs?: { running: number; queued: number; failed_24h?: number } | null;
  /** Latest live sample, if the monitor is running. */
  current?: MetricSample | null;
}

export type MetricsWindow = '1h' | '24h' | '7d';

/** GET /api/v2/system/metrics?window= */
export interface MetricsSeries {
  window?: MetricsWindow;
  samples: MetricSample[];
}

/** Row of the `errors` table. GET /api/v2/errors */
export interface ErrorRow {
  fingerprint: string;
  source: string;
  message: string;
  context?: Record<string, unknown> | string | null;
  count: number;
  first_seen: string;
  last_seen: string;
  resolved_at: string | null;
}

// ---- jobs ----

/** `job_runs.resources`: the whole process, sampled while the job runs (R22). */
export interface JobResources {
  peak_rss_mb?: number;
  avg_cpu_pct?: number;
  cpu_secs?: number;
  wall_ms?: number;
  /** Always "process": figures include any job that ran at the same time. */
  scope?: string;
  /** Most jobs metered at once during this job (this one included). */
  concurrent_jobs_max?: number;
}

/** GET /api/v2/jobs row: a v1 JobRun plus v2 columns. */
export interface JobRunV2 extends Omit<JobRun, 'kind'> {
  kind: string;
  site_domain?: string | null;
  attempts?: number;
  max_attempts?: number;
  priority?: number;
  requested_by?: string | null;
  resources?: JobResources | null;
  payload?: Record<string, unknown> | null;
}

export interface JobsQuery {
  status?: string;
  kind?: string;
  site_id?: string;
  limit?: number;
}

export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

/** GET /api/v2/jobs/:id/logs?after_id= rows */
export interface JobLogRow {
  id: number;
  ts: string;
  level: LogLevel | string;
  message: string;
  data: unknown;
}

// ---- notifications ----

export interface NotificationRow {
  id: number;
  event: string;
  level: string;
  title: string;
  body: string;
  data?: unknown;
  site_id?: string | null;
  job_run_id?: string | null;
  read_at: string | null;
  created_at: string;
}

export interface NotificationsList {
  notifications: NotificationRow[];
  /** Total unread count (may exceed the rows returned). */
  unread?: number;
}

export type ChannelKind = 'webhook' | 'ntfy' | 'telegram' | 'hermes' | 'desktop' | 'log';

export const CHANNEL_KINDS: ChannelKind[] = ['webhook', 'ntfy', 'telegram', 'hermes', 'desktop', 'log'];

export const NOTIFY_EVENTS = [
  'job.failed',
  'job.done',
  'plan.ready',
  'inject.done',
  'backup.done',
  'backup.failed',
  'system.alert',
] as const;

export interface NotificationChannel {
  id: string;
  name: string;
  kind: ChannelKind;
  /** Kind-specific settings. Secret keys are write-only: the server omits or masks them. */
  config: Record<string, unknown>;
  events: string[];
  enabled: boolean;
  created_at?: string;
}

export interface ChannelInput {
  name: string;
  kind: ChannelKind;
  config: Record<string, unknown>;
  events: string[];
  enabled: boolean;
}

export interface ChannelTestResult {
  ok: boolean;
  error?: string | null;
}

// ---- backups ----

export type BackupKind = 'full' | 'db' | 'config';

export interface BackupRow {
  id: string;
  created_at: string;
  kind: BackupKind | string;
  size_bytes: number;
  encrypted: boolean;
  app_version: string;
  /** 'ok' | 'running' | 'failed' … (missing = ok). */
  status?: string;
  note: string;
  /** SHA-256 of the archive file ('' while running). */
  sha256?: string;
}

/** `POST /api/v2/backups/{id}/verify`: the archive manifest on success. */
export interface BackupVerifyResult {
  ok: boolean;
  error?: string | null;
  manifest?: {
    format?: number;
    app_version?: string;
    schema_version?: number;
    created_at?: string;
    kind?: string;
    secrets_excluded?: boolean;
    [k: string]: unknown;
  };
}

// ---- API tokens ----

export type TokenScope = '*' | 'read' | 'write' | 'admin';

export interface ApiTokenRow {
  id: string;
  name: string;
  prefix: string;
  scopes: string[];
  created_at: string;
  last_used_at: string | null;
  expires_at: string | null;
  revoked_at: string | null;
}

/** POST /api/v2/tokens response: the row plus the plaintext token, shown once. */
export interface ApiTokenCreated extends ApiTokenRow {
  token: string;
}

// ---- MCP ----

export interface McpTool {
  name: string;
  description?: string;
}

/** GET /api/v2/mcp */
export interface McpInfo {
  enabled?: boolean;
  /** Absolute path of the siloforge executable (stdio transport). */
  stdio_command: string;
  stdio_args?: string[];
  /** Streamable-HTTP endpoint, e.g. http://127.0.0.1:8600/mcp */
  http_url: string;
  /** Base URL clients should use for SILOFORGE_API_URL. */
  api_url?: string;
  tools?: McpTool[];
}

// ---- audit ----

export interface AuditRow {
  id: number;
  ts: string;
  actor: string;
  via: string;
  action: string;
  target: string | null;
  detail: Record<string, unknown> | string | null;
}
