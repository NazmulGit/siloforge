// Local mirror of the shapes the UI consumes.
// Source of truth: packages/shared/src/types.ts + CONTRACTS.md (API routes section).
// The UI package intentionally does NOT depend on @siloforge/shared (slim Docker build).

export type SiteMode = 'wp_rest' | 'sitemap';
/** `naked` = destination URL used verbatim; `cta` = call-to-action phrasing (checklist C5). */
export type AnchorType = 'exact' | 'partial' | 'branded' | 'generic' | 'lsi' | 'naked' | 'cta';
/** `bidirectional` = pillar ↔ every spoke, both directions (checklist A3). */
export type SiloTopology = 'reverse' | 'serial' | 'priority' | 'circle' | 'bidirectional';
export type InsertModule = 'native' | 'rephrase' | 'context' | 'content_gap';
export type EdgeStatus = 'suggested' | 'approved' | 'rejected' | 'applied' | 'reverted' | 'failed';
export type JobKind =
  | 'crawl'
  | 'plan'
  | 'inject'
  | 'undo'
  | 'keyword_link'
  | 'link_health'
  | 'audit';
export type JobStatus = 'queued' | 'running' | 'done' | 'failed' | 'cancelled';

export const SILO_TOPOLOGIES: SiloTopology[] = [
  'reverse',
  'serial',
  'priority',
  'circle',
  'bidirectional',
];

/** One-line explanation of each silo shape, reused by every topology selector. */
export const TOPOLOGY_HINTS: Record<SiloTopology, string> = {
  reverse: 'support posts → pillar (equity flows up to the money page)',
  serial: 'a chain — each post links to the next in the cluster',
  priority: 'concentrate equity on your highest-priority pages',
  circle: 'a loop — every post links onward, last links back to first',
  bidirectional: 'pillar ↔ every spoke, both directions',
};

export const ANCHOR_TYPES: AnchorType[] = [
  'exact',
  'partial',
  'branded',
  'generic',
  'lsi',
  'naked',
  'cta',
];

/** What each anchor class means — surfaced in filters and distribution panels. */
export const ANCHOR_TYPE_HINTS: Record<AnchorType, string> = {
  exact: 'the exact target keyword',
  partial: 'the keyword inside a longer phrase',
  branded: 'your brand or site name',
  generic: '“click here”, “read more” and friends',
  lsi: 'a semantically related variation',
  naked: 'the destination URL used verbatim as the anchor',
  cta: 'call-to-action phrasing (“get the checklist”)',
};

export const EDGE_STATUSES: EdgeStatus[] = [
  'suggested',
  'approved',
  'applied',
  'rejected',
  'failed',
  'reverted',
];

export interface SiteSettings {
  preset?: 'conservative' | 'balanced' | 'aggressive';
  silo_topology?: SiloTopology;
  insert_modules?: InsertModule[];
  /** Hard ceiling on contextual internal links a single post may carry (checklist D2). */
  max_links_per_post?: number;
  /** Target floor used to flag under-linked posts (T8 SOP: >= 3). */
  min_links_per_post?: number;
  /** Max NEW links injected into one post in a single pass (T8 SOP "never do": 5). */
  max_new_links_per_pass?: number;
  /** Outbound-link count above which a page is flagged as link-overloaded (checklist D4). */
  link_overload_threshold?: number;
  money_page_urls?: string[];
  /** Skip the post-injection live verify. Off by default — the T8 SOP calls it mandatory. */
  skip_inject_verify?: boolean;
}

// ---- Rules layer (LinkBoss parity) ----
export interface KeywordRule {
  id: string;
  site_id: string;
  keyword: string;
  target_url: string;
  anchor_type: AnchorType;
  max_links: number;
  active: boolean;
  created_at: string;
}

export interface IgnoreKeyword {
  id: string;
  site_id: string;
  phrase: string;
  created_at: string;
}

export interface AnchorReport {
  total: number;
  byType: Partial<Record<AnchorType, number>>;
  byAnchor: Array<{ anchor: string; count: number }>;
  warnings: string[];
}

export interface Site {
  id: string;
  domain: string;
  wp_url: string | null;
  mode: SiteMode;
  api_key: string;
  settings: SiteSettings;
  created_at: string;
  last_sync_at: string | null;
}

/** GET /sites returns Site[] "with post/edge counts" — field names tolerated defensively. */
export interface SiteListItem extends Site {
  posts_count?: number;
  post_count?: number;
  posts?: number;
  edge_counts?: Partial<Record<EdgeStatus, number>>;
  edges_by_status?: Partial<Record<EdgeStatus, number>>;
}

export function sitePostCount(s: SiteListItem): number {
  return s.posts_count ?? s.post_count ?? s.posts ?? 0;
}

export function siteEdgeCounts(s: SiteListItem): Partial<Record<EdgeStatus, number>> {
  return s.edges_by_status ?? s.edge_counts ?? {};
}

/** The 11 scoring dimensions, each normalized 0..1 before weighting. */
export interface ScoreBreakdown {
  semantic: number;
  orphan: number;
  depth: number;
  tier: number;
  cluster: number;
  velocity: number;
  graph: number;
  money: number;
  gsc: number;
  authority: number;
  placement: number;
  weights: Record<string, number>;
}

export interface LinkEdge {
  id: string;
  site_id: string;
  source_post_id: string;
  target_post_id: string;
  paragraph_id: string | null;
  anchor: string;
  anchor_type: AnchorType;
  composite_score: number;
  score_breakdown: ScoreBreakdown | Record<string, never>;
  silo_topology: SiloTopology | null;
  insert_module: InsertModule | null;
  status: EdgeStatus;
  origin: string;
  snapshot_id: string | null;
  error: string | null;
  created_at: string;
  decided_at: string | null;
  applied_at: string | null;
}

/** EdgeView = LinkEdge + source/target url+title (per CONTRACTS.md). */
export interface EdgeView extends LinkEdge {
  source_url: string;
  source_title: string;
  target_url: string;
  target_title: string;
}

export interface JobRun {
  id: string;
  site_id: string | null;
  kind: JobKind;
  status: JobStatus;
  stats: Record<string, unknown>;
  error: string | null;
  started_at: string | null;
  finished_at: string | null;
  created_at: string;
}

export interface CannibalizationWarningObj {
  keyword?: string;
  anchor?: string;
  message?: string;
  posts?: Array<string | { url?: string; title?: string }>;
  urls?: string[];
  target_a_url?: string;
  target_a_title?: string;
  target_b_url?: string;
  target_b_title?: string;
}
export type CannibalizationWarning = string | CannibalizationWarningObj;

export interface OrphanPost {
  post_id: string;
  url: string;
  title: string;
}

export interface DashboardData {
  site: Site;
  health_score: number;
  posts: number;
  orphans: number;
  dead_ends: number;
  avg_links_per_post: number;
  edges_by_status: Partial<Record<EdgeStatus, number>>;
  clusters: number;
  cannibalization_warnings: CannibalizationWarning[];
  recent_jobs: JobRun[];
  // LinkBoss parity — dashboard cards (Gap E/F/G).
  opportunities: number;
  http_links: number;
  outbound_domains: number;
  link_duplication: number;
  orphan_posts: OrphanPost[];
}

export interface LinkReportRow {
  post_id: string;
  url: string;
  title: string;
  inbound: number;
  outbound: number;
  pagerank: number;
  click_depth: number;
  link_flow_score: number;
  /** Gap I — extra duplicate link edges touching this post (0 = none). */
  duplicate_count: number;
}

/** Gap J — Link History row (applied/reverted edges). */
export interface HistoryRow {
  edge_id: string;
  source_url: string;
  source_title: string;
  target_url: string;
  target_title: string;
  anchor: string;
  anchor_type: AnchorType;
  tool: string;
  status: EdgeStatus;
  applied_at: string | null;
  decided_at: string | null;
}

export interface HistoryResponse {
  rows: HistoryRow[];
  total: number;
  limit: number;
  offset: number;
}

export interface GraphReportNode {
  id: string;
  url: string;
  title: string;
  cluster_id: string | null;
  is_orphan: boolean;
  pagerank: number;
}

export interface GraphReportEdge {
  source: string;
  target: string;
  status: EdgeStatus;
}

export interface GraphReport {
  nodes: GraphReportNode[];
  edges: GraphReportEdge[];
  /** Total graph size before server-side level-of-detail limits. */
  total_nodes?: number;
  total_edges?: number;
  node_limit?: number;
  edge_limit?: number;
  truncated?: boolean;
}

// ---- Custom Silo Network (Gap B) ----
export interface ClusterInfo {
  id: string;
  label: string;
  source: 'auto' | 'custom';
  pillar_url: string | null;
  members: number;
}

export interface ClusterExportEntry {
  label: string;
  pillar_url: string | null;
  member_urls: string[];
}

export interface ClusterExport {
  clusters: ClusterExportEntry[];
}

export interface ClusterImportResult {
  clusters: number;
  members: number;
  skipped: number;
}

// ---- CSV connector (Gap K-CSV) ----
export interface CsvImportResult {
  received: number;
  upserted: number;
  skipped: number;
}

// ---- Bulk Wizard: source post list (GET /posts/:siteId) ----
export interface PostListItem {
  id: string;
  url: string;
  title: string;
  post_type: string;
  cluster_id: string | null;
  cluster_label: string | null;
  is_orphan: boolean;
  is_dead_end: boolean;
  /** Count of suggested outbound edges already scored for this post. */
  suggested_out: number;
  /** Count of suggested inbound edges already scored for this post. */
  suggested_in: number;
}

/** A topical cluster option for the Fetch scope selector (the "topical map"). */
export interface PostsClusterOption {
  id: string;
  label: string;
  count: number;
}

export interface PostsResponse {
  posts: PostListItem[];
  total: number;
  post_types: string[];
  clusters: PostsClusterOption[];
}

/** stats.progress emitted by plan/inject job runs while they execute. */
export interface JobProgress {
  phase: string;
  pct: number;
}

// ---------------------------------------------------------------------------
// Link health (checklist E) — GET /reports/link-health/:siteId
// ---------------------------------------------------------------------------

/** One hop of a redirect chain. `order` 0 is the requested URL. */
export interface RedirectHop {
  url: string;
  status: number;
  order: number;
}

export type LinkPlacement = 'body' | 'nav' | 'header' | 'footer' | 'aside' | 'unknown';

/**
 * A link_health row as the REPORT returns it: the API joins the source post's
 * url/title on and omits the columns the UI has no use for (site_id, ok,
 * target_post_id). Shared's `LinkHealthRow` is the storage shape — this is the
 * wire shape.
 */
export interface LinkHealthRow {
  id: string;
  source_post_id: string | null;
  source_url: string | null;
  source_title: string | null;
  href: string;
  requested_url: string;
  resolved_url: string | null;
  scope: 'internal' | 'external';
  status_code: number | null;
  is_broken: boolean;
  redirect_hops: number;
  /** JSONB — always read through `redirectHops()`, never trusted as-is. */
  redirect_chain: unknown;
  has_redirect_loop: boolean;
  has_temp_redirect: boolean;
  rel: string;
  is_nofollow: boolean;
  is_sponsored: boolean;
  is_ugc: boolean;
  anchor: string;
  placement: LinkPlacement;
  error: string | null;
  checked_at: string;
}

/** Defensive read of a `redirect_chain` JSONB column into ordered hops. */
export function redirectHops(raw: unknown): RedirectHop[] {
  if (!Array.isArray(raw)) return [];
  const hops: RedirectHop[] = [];
  raw.forEach((item, i) => {
    if (item === null || typeof item !== 'object') return;
    const h = item as { url?: unknown; status?: unknown; order?: unknown };
    if (typeof h.url !== 'string') return;
    hops.push({
      url: h.url,
      status: typeof h.status === 'number' ? h.status : 0,
      order: typeof h.order === 'number' ? h.order : i,
    });
  });
  return hops.sort((a, b) => a.order - b.order);
}

export interface LinkHealthSummary {
  total_links: number;
  internal: number;
  external: number;
  broken: number;
  redirected: number;
  chains_over_two: number;
  loops: number;
  temp_redirects: number;
  nofollow: number;
  sponsored: number;
  ugc: number;
  last_checked_at: string | null;
}

export interface BreadcrumbPostRow {
  post_id: string;
  url: string;
  title: string;
  has_breadcrumbs: boolean;
  has_breadcrumb_schema: boolean;
  breadcrumb_report: unknown;
}

export interface BreadcrumbReport {
  posts: number;
  checked: number;
  with_visible: number;
  with_schema: number;
  mismatched: number;
  missing: BreadcrumbPostRow[];
}

export interface LinkHealthReport {
  summary: LinkHealthSummary;
  broken: LinkHealthRow[];
  redirects: LinkHealthRow[];
  nofollow: LinkHealthRow[];
  breadcrumbs: BreadcrumbReport;
  limit: number;
  offset: number;
}

// ---------------------------------------------------------------------------
// Schedules + metric history (checklist F)
// ---------------------------------------------------------------------------

export type ScheduleKind = 'crawl' | 'plan' | 'link_health' | 'audit';

export const SCHEDULE_KINDS: ScheduleKind[] = ['audit', 'crawl', 'plan', 'link_health'];

export interface SiteSchedule {
  id: string;
  site_id: string;
  job_kind: ScheduleKind;
  cron: string;
  tz: string;
  active: boolean;
  payload: Record<string, unknown>;
  last_run_at: string | null;
  last_job_run_id: string | null;
  next_run_at: string | null;
  created_at: string;
  updated_at: string;
}

/** POST /schedules/:siteId echoes the row plus whether BullMQ accepted it. */
export interface SavedSchedule extends SiteSchedule {
  scheduled?: boolean;
  warning?: string;
}

/** Every field optional so the snapshot shape can grow without a migration. */
export interface SiteMetrics {
  posts?: number;
  orphans?: number;
  /** no in-body editorial inbound link (nav still makes it crawlable) */
  contextual_orphans?: number;
  /** reachable ONLY via nav/header/footer/aside */
  nav_only_orphans?: number;
  dead_ends?: number;
  avg_click_depth?: number;
  max_click_depth?: number;
  components?: number;
  internal_links?: number;
  broken_links?: number;
  nofollow_links?: number;
  redirect_links?: number;
  redirect_chains_over_2?: number;
  link_overload_pages?: number;
  avg_links_per_post?: number;
  posts_under_min_links?: number;
  clusters?: number;
  edges_suggested?: number;
  edges_applied?: number;
  health_score?: number;
}

export type MetricKey = keyof SiteMetrics;

export interface SiteMetricsSnapshot {
  id: string;
  site_id: string;
  job_run_id: string | null;
  source: 'crawl' | 'plan' | 'link_health' | 'audit' | 'manual';
  metrics: SiteMetrics;
  captured_at: string;
}

/**
 * A metric compared against the previous snapshot. `delta` is current - previous.
 * `improved` is authoritative — NEVER re-derive direction from the sign of the
 * delta (fewer orphans is an improvement even though the number went down).
 */
export interface MetricDelta {
  key: MetricKey;
  current: number | null;
  previous: number | null;
  delta: number | null;
  improved: boolean | null;
  /** null when the baseline was 0 or absent. */
  pct?: number | null;
}

export interface MetricsReport {
  latest: SiteMetricsSnapshot | null;
  deltas: MetricDelta[];
  trend: SiteMetricsSnapshot[];
}

/** Human labels for the metric keys we surface. */
export const METRIC_LABELS: Partial<Record<MetricKey, string>> = {
  posts: 'Posts',
  orphans: 'Orphans',
  contextual_orphans: 'Contextual orphans',
  nav_only_orphans: 'Nav-only orphans',
  dead_ends: 'Dead ends',
  avg_click_depth: 'Avg click depth',
  max_click_depth: 'Max click depth',
  components: 'Disconnected components',
  internal_links: 'Internal links',
  broken_links: 'Broken links',
  nofollow_links: 'Nofollow internal links',
  redirect_links: 'Redirected links',
  redirect_chains_over_2: 'Redirect chains > 2 hops',
  link_overload_pages: 'Link-overloaded pages',
  avg_links_per_post: 'Avg links / post',
  posts_under_min_links: 'Posts under min links',
  clusters: 'Clusters',
  edges_suggested: 'Suggested edges',
  edges_applied: 'Applied edges',
  health_score: 'Health score',
};

// ---------------------------------------------------------------------------
// Model providers / fallback chains (Settings → Models, admin only)
// ---------------------------------------------------------------------------

export type ModelRole = 'chat' | 'embed';
export type ProviderKind = 'openai-compatible' | 'anthropic' | 'openrouter' | 'ollama' | 'custom';

export const PROVIDER_KINDS: ProviderKind[] = [
  'openai-compatible',
  'anthropic',
  'openrouter',
  'ollama',
  'custom',
];

export const MODEL_ROLES: ModelRole[] = ['chat', 'embed'];

export interface ModelProvider {
  id: string;
  name: string;
  label: string;
  kind: ProviderKind;
  base_url: string;
  /** never the ciphertext and never the plaintext — a masked hint like `****ab12` */
  api_key_masked: string;
  has_api_key: boolean;
  headers: Record<string, string>;
  enabled: boolean;
  last_probe_at: string | null;
  last_probe_ok: boolean | null;
  last_probe_error: string | null;
  last_probe_ms: number | null;
  created_at: string;
  updated_at: string;
}

export interface ModelRow {
  id: string;
  provider_id: string;
  model_id: string;
  label: string;
  role: ModelRole;
  context_tokens: number | null;
  /** embed models: output width; must match the live index width to be chainable */
  dim: number | null;
  enabled: boolean;
  price_in: number | null;
  price_out: number | null;
  notes: string;
  last_probe_at: string | null;
  last_probe_ok: boolean | null;
  last_probe_error: string | null;
  last_probe_ms: number | null;
  created_at: string;
}

export interface ModelChain {
  id: string;
  name: string;
  role: ModelRole;
  /** null = global default chain for the role */
  site_id: string | null;
  active: boolean;
  notes: string;
  created_at: string;
  updated_at: string;
}

export interface ModelChainRung {
  id: string;
  chain_id: string;
  /** the models-table row id (NOT the wire model name) */
  model_id: string;
  position: number;
  timeout_ms: number | null;
  max_retries: number | null;
  enabled: boolean;
}

/** Rung as GET /model-chains returns it — joined with model + provider detail. */
export interface ChainRungView extends ModelChainRung {
  wire_model_id: string;
  provider_name: string;
  model_role: ModelRole;
  dim: number | null;
}

export interface ModelChainWithRungs extends ModelChain {
  rungs: ChainRungView[];
}

export interface ProbeResult {
  ok: boolean;
  latency_ms: number;
  status_code: number | null;
  error: string | null;
  /** embed probes: the vector width actually returned */
  dim?: number | null;
  /** chat probes: first characters of the reply */
  sample?: string | null;
  /** model probes echo the live vector-index width so the UI can compare */
  index_dim?: number;
}

export interface ProviderHealth extends ModelProvider {
  models: ModelRow[];
}

export interface ProvidersHealthReport {
  providers: ProviderHealth[];
  index_dim: number;
}
