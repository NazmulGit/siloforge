// Bulk Wizard (LinkBoss "Bulk 2.0" parity) — a guided multi-step flow:
//   Fetch → Match → Progress → Results → Link
// Lives inside the Tools "Bulk" tab. Talks only to the documented API endpoints.
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  bulkDecide,
  bulkRun,
  ApiError,
  fetchAllEdges,
  fetchJobRun,
  fetchJobs,
  fetchPosts,
  triggerPlan,
  type PostsQuery,
} from '../api';
import { scoreTone } from '../format';
import { useToast } from '../toast';
import {
  SILO_TOPOLOGIES,
  TOPOLOGY_HINTS,
  type EdgeView,
  type JobProgress,
  type JobRun,
  type PostListItem,
  type PostsClusterOption,
  type SiloTopology,
} from '../types';
import { EmptyState, Skeleton } from './Skeleton';
import { StatusChip } from './StatusChip';

type WizardStep = 'fetch' | 'match' | 'progress' | 'results' | 'link';
type ScopeMode = 'all' | 'cluster' | 'post_type' | 'search';

const STEPS: Array<{ id: WizardStep; label: string }> = [
  { id: 'fetch', label: 'Fetch' },
  { id: 'match', label: 'Match' },
  { id: 'progress', label: 'Progress' },
  { id: 'results', label: 'Results' },
  { id: 'link', label: 'Link' },
];

const POST_FETCH_LIMIT = 500;

// ---- URL-import parsing (paste or .csv/.json upload) ----

/** Normalize a URL for set membership: drop scheme, lowercase, strip trailing slash. */
function normUrl(u: string): string {
  return u
    .trim()
    .toLowerCase()
    .replace(/^https?:\/\//, '')
    .replace(/\/+$/, '');
}

/** Parse a pasted/uploaded blob (newline / CSV / JSON) into a list of URLs. */
function parseUrls(text: string): string[] {
  const trimmed = text.trim();
  if (!trimmed) return [];
  if (trimmed.startsWith('[') || trimmed.startsWith('{')) {
    try {
      const data: unknown = JSON.parse(trimmed);
      const arr: unknown[] = Array.isArray(data)
        ? data
        : Array.isArray((data as { urls?: unknown[] }).urls)
          ? (data as { urls: unknown[] }).urls
          : Array.isArray((data as { posts?: unknown[] }).posts)
            ? (data as { posts: unknown[] }).posts
            : [];
      return arr
        .map((x) => (typeof x === 'string' ? x : ((x as { url?: string })?.url ?? '')))
        .filter((s): s is string => typeof s === 'string' && s.length > 0);
    } catch {
      /* not valid JSON — fall through to line/CSV parsing */
    }
  }
  return trimmed
    .split(/[\r\n,]+/)
    .map((s) => s.trim())
    .filter((t) => /^https?:\/\//i.test(t) || t.includes('/'));
}

/** Read stats.progress off a job run defensively. */
function readProgress(job: JobRun): JobProgress | null {
  const p = (job.stats as { progress?: unknown }).progress;
  if (p && typeof p === 'object') {
    const obj = p as { phase?: unknown; pct?: unknown };
    return {
      phase: typeof obj.phase === 'string' ? obj.phase : '',
      pct: typeof obj.pct === 'number' ? obj.pct : 0,
    };
  }
  return null;
}

function scorePct(score: number): number {
  return Math.round(Math.max(0, Math.min(1, score)) * 100);
}

function download(name: string, mime: string, body: string): void {
  const blob = new Blob([body], { type: mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

function csvCell(v: string): string {
  return /[",\n]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v;
}

// ---- progress panel (shared by plan + inject) ----

function ProgressPanel({ progress, title }: { progress: JobProgress | null; title: string }) {
  const pct = progress ? Math.max(0, Math.min(100, Math.round(progress.pct))) : 0;
  const phase = progress?.phase || 'Starting…';
  return (
    <div className="card wiz-progress">
      <div className="card-title">{title}</div>
      <div className="wiz-progress-label">
        <span>{phase}</span>
        <span className="mono">{pct}%</span>
      </div>
      <div className="wiz-bar">
        <div className="wiz-bar-fill" style={{ width: `${pct}%` }} />
      </div>
      <div className="faint" style={{ marginTop: 10, fontSize: 12.5 }}>
        Semantic NLP embeddings + topical-map relevancy scoring across the 11 dimensions…
      </div>
    </div>
  );
}

// ---- stepper header ----

function Stepper({ current }: { current: WizardStep }) {
  const idx = STEPS.findIndex((s) => s.id === current);
  return (
    <div className="wiz-steps">
      {STEPS.map((s, i) => (
        <div
          key={s.id}
          className={`wiz-step${i === idx ? ' active' : ''}${i < idx ? ' done' : ''}`}
        >
          <span className="wiz-step-num">{i < idx ? '✓' : i + 1}</span>
          <span className="wiz-step-label">{s.label}</span>
        </div>
      ))}
    </div>
  );
}

// ---- results table (clickable source → target links) ----

interface ResultsTableProps {
  edges: EdgeView[];
  selected: ReadonlySet<string>;
  onToggle: (id: string) => void;
  onToggleAll: () => void;
  onDecide: (ids: string[], decision: 'approved' | 'rejected') => void;
  busy: boolean;
}

function ResultsTable({ edges, selected, onToggle, onToggleAll, onDecide, busy }: ResultsTableProps) {
  const sorted = useMemo(() => {
    return [...edges].sort(
      (a, b) => a.source_title.localeCompare(b.source_title) || b.composite_score - a.composite_score,
    );
  }, [edges]);

  const groupCounts = useMemo(() => {
    const m = new Map<string, number>();
    for (const e of sorted) m.set(e.source_post_id, (m.get(e.source_post_id) ?? 0) + 1);
    return m;
  }, [sorted]);

  const allSelected = sorted.length > 0 && sorted.every((e) => selected.has(e.id));
  let prevKey: string | null = null;

  return (
    <div className="tbl-wrap">
      <table className="tbl">
        <thead>
          <tr>
            <th style={{ width: 34 }}>
              <input
                type="checkbox"
                checked={allSelected}
                onChange={onToggleAll}
                aria-label="Select all results"
              />
            </th>
            <th>Source → Target</th>
            <th>Anchor</th>
            <th>Relevancy</th>
            <th>Module</th>
            <th>Status</th>
            <th>Actions</th>
          </tr>
        </thead>
        <tbody>
          {sorted.map((e) => {
            const rows = [];
            if (e.source_post_id !== prevKey) {
              prevKey = e.source_post_id;
              const count = groupCounts.get(e.source_post_id) ?? 0;
              rows.push(
                <tr key={`g-${e.source_post_id}`} className="group-row">
                  <td colSpan={7}>
                    Links from:{' '}
                    <a href={e.source_url} target="_blank" rel="noopener noreferrer">
                      {e.source_title || '(untitled)'}
                    </a>{' '}
                    <span className="muted">
                      · {count} match{count === 1 ? '' : 'es'}
                    </span>
                  </td>
                </tr>,
              );
            }
            const pct = scorePct(e.composite_score);
            rows.push(
              <tr key={e.id}>
                <td>
                  <input
                    type="checkbox"
                    checked={selected.has(e.id)}
                    onChange={() => onToggle(e.id)}
                    aria-label="Select match"
                  />
                </td>
                <td className="pair-cell">
                  <div className="pair">
                    <a className="src" href={e.source_url} target="_blank" rel="noopener noreferrer">
                      {e.source_title || '(untitled)'}
                    </a>
                    <span className="arrow">→</span>
                    <a className="dst" href={e.target_url} target="_blank" rel="noopener noreferrer">
                      {e.target_title || '(untitled)'}
                    </a>
                  </div>
                  <div className="pair-urls" title={`${e.source_url} → ${e.target_url}`}>
                    {e.source_url} → {e.target_url}
                  </div>
                </td>
                <td className="anchor-cell" title={e.anchor}>
                  “{e.anchor}”
                </td>
                <td>
                  <span className={`scorebar scorebar-${scoreTone(pct)}`}>
                    <span className="scorebar-num">{pct}</span>
                    <span className="scorebar-track">
                      <span className="scorebar-fill" style={{ width: `${pct}%` }} />
                    </span>
                  </span>
                </td>
                <td className="muted">{e.insert_module ?? '—'}</td>
                <td>
                  <StatusChip status={e.status} />
                </td>
                <td>
                  {e.status === 'suggested' ? (
                    <div className="row-actions">
                      <button
                        className="btn btn-sm btn-primary"
                        disabled={busy}
                        onClick={() => onDecide([e.id], 'approved')}
                      >
                        Approve
                      </button>
                      <button
                        className="btn btn-sm btn-danger"
                        disabled={busy}
                        onClick={() => onDecide([e.id], 'rejected')}
                      >
                        Reject
                      </button>
                    </div>
                  ) : (
                    <span className="faint">—</span>
                  )}
                </td>
              </tr>,
            );
            return rows;
          })}
        </tbody>
      </table>
    </div>
  );
}

// ---- the wizard ----

export interface BulkWizardProps {
  siteId: string;
  onClose: () => void;
  /** Called after a successful inject so the parent Bulk tab can refresh. */
  onApplied?: () => void;
}

export function BulkWizard({ siteId, onClose, onApplied }: BulkWizardProps) {
  const toast = useToast();

  const [step, setStep] = useState<WizardStep>('fetch');

  // ---- Step 1: Fetch ----
  const [scope, setScope] = useState<ScopeMode>('all');
  const [clusterId, setClusterId] = useState('');
  const [postType, setPostType] = useState('');
  const [search, setSearch] = useState('');
  const [orphansOnly, setOrphansOnly] = useState(false);

  const [posts, setPosts] = useState<PostListItem[]>([]);
  const [postTotal, setPostTotal] = useState(0);
  const [clusters, setClusters] = useState<PostsClusterOption[]>([]);
  const [postTypes, setPostTypes] = useState<string[]>([]);
  const [postsLoading, setPostsLoading] = useState(false);
  const [postsError, setPostsError] = useState<string | null>(null);
  const [fetched, setFetched] = useState(false);

  const [selectedPosts, setSelectedPosts] = useState<ReadonlySet<string>>(new Set());

  // URL-import filter
  const [importText, setImportText] = useState('');
  const [importSet, setImportSet] = useState<ReadonlySet<string> | null>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  // ---- Step 2: matching options ----
  const [topology, setTopology] = useState<SiloTopology | ''>('');

  // ---- Step 3/5: job progress ----
  const [runningJob, setRunningJob] = useState<{ id: string; type: 'plan' | 'inject' } | null>(null);
  const [progress, setProgress] = useState<JobProgress | null>(null);
  const [jobError, setJobError] = useState<string | null>(null);

  // ---- Step 4: Results ----
  const [edges, setEdges] = useState<EdgeView[]>([]);
  const [edgesLoading, setEdgesLoading] = useState(false);
  const [edgesError, setEdgesError] = useState<string | null>(null);
  const [threshold, setThreshold] = useState(0);
  const [selectedEdges, setSelectedEdges] = useState<ReadonlySet<string>>(new Set());
  const [decideBusy, setDecideBusy] = useState(false);

  // ---- Step 5: summary ----
  const [appliedCount, setAppliedCount] = useState<number | null>(null);

  const buildQuery = useCallback((): PostsQuery => {
    const q: PostsQuery = { limit: POST_FETCH_LIMIT };
    if (scope === 'cluster' && clusterId) q.cluster_id = clusterId;
    if (scope === 'post_type' && postType) q.post_type = postType;
    if (scope === 'search' && search.trim()) q.search = search.trim();
    if (orphansOnly) q.is_orphan = true;
    return q;
  }, [scope, clusterId, postType, search, orphansOnly]);

  const runFetch = useCallback(async () => {
    setPostsLoading(true);
    setPostsError(null);
    try {
      const res = await fetchPosts(siteId, buildQuery());
      setPosts(res.posts);
      setPostTotal(res.total);
      setClusters(res.clusters);
      setPostTypes(res.post_types);
      setSelectedPosts(new Set());
      setFetched(true);
    } catch (e) {
      setPostsError(e instanceof Error ? e.message : 'Failed to fetch posts');
      setPosts([]);
    } finally {
      setPostsLoading(false);
    }
  }, [siteId, buildQuery]);

  // Initial fetch on mount (All posts) so the table isn't empty.
  useEffect(() => {
    void runFetch();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Posts after the client-side URL-import filter.
  const visiblePosts = useMemo(() => {
    if (!importSet) return posts;
    return posts.filter((p) => importSet.has(normUrl(p.url)));
  }, [posts, importSet]);

  const allPostsSelected =
    visiblePosts.length > 0 && visiblePosts.every((p) => selectedPosts.has(p.id));

  function togglePost(id: string) {
    const next = new Set(selectedPosts);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelectedPosts(next);
  }

  function toggleAllPosts() {
    if (allPostsSelected) {
      setSelectedPosts(new Set());
    } else {
      setSelectedPosts(new Set(visiblePosts.map((p) => p.id)));
    }
  }

  function applyImport(text: string) {
    const urls = parseUrls(text);
    if (urls.length === 0) {
      toast.push('No URLs found in the pasted/loaded content', 'error');
      return;
    }
    setImportSet(new Set(urls.map(normUrl)));
    toast.push(`URL filter applied · ${urls.length} URL${urls.length === 1 ? '' : 's'}`, 'success');
  }

  async function onImportFile(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    if (!file) return;
    try {
      const text = await file.text();
      setImportText(text);
      applyImport(text);
    } catch {
      toast.push('Could not read file', 'error');
    } finally {
      if (fileRef.current) fileRef.current.value = '';
    }
  }

  const selectedPostList = useMemo(
    () => posts.filter((p) => selectedPosts.has(p.id)),
    [posts, selectedPosts],
  );
  const selectedSuggestedOut = useMemo(
    () => selectedPostList.reduce((n, p) => n + (p.suggested_out ?? 0), 0),
    [selectedPostList],
  );
  const hasExistingSuggestions = selectedSuggestedOut > 0;

  // ---- results loading (filtered to the selected source posts) ----
  const loadResults = useCallback(async () => {
    setEdgesLoading(true);
    setEdgesError(null);
    try {
      // Page through every suggestion (v1 caps a page at 1000) so large sites aren't truncated.
      const res = await fetchAllEdges(siteId, { status: 'suggested' });
      const filtered = res.edges.filter((e) => selectedPosts.has(e.source_post_id));
      setEdges(filtered);
      setSelectedEdges(new Set());
    } catch (e) {
      setEdgesError(e instanceof Error ? e.message : 'Failed to load results');
      setEdges([]);
    } finally {
      setEdgesLoading(false);
    }
  }, [siteId, selectedPosts]);

  // ---- job progress polling (plan or inject) ----
  useEffect(() => {
    if (!runningJob) return;
    let cancelled = false;
    let useList = false; // older servers lack GET /jobs/run/:id
    const timer = window.setInterval(() => {
      void (async () => {
        let job: JobRun | undefined;
        try {
          if (!useList) {
            try {
              job = await fetchJobRun(runningJob.id);
            } catch (e) {
              if (!(e instanceof ApiError && e.status === 404)) throw e;
              useList = true;
            }
          }
          if (useList) {
            const jobs = await fetchJobs(siteId);
            job = jobs.find((j) => j.id === runningJob.id);
          }
        } catch {
          return; // transient — keep polling
        }
        if (!job || cancelled) return;
        const prog = readProgress(job);
        if (prog) setProgress(prog);
        if (job.status === 'done') {
          window.clearInterval(timer);
          if (cancelled) return;
          setProgress({ phase: 'Done', pct: 100 });
          if (runningJob.type === 'plan') {
            toast.push('Topical matching complete', 'success');
            setRunningJob(null);
            setStep('results');
            void loadResults();
          } else {
            const applied = (job.stats as { applied?: unknown }).applied;
            setAppliedCount(typeof applied === 'number' ? applied : null);
            toast.push('Links applied', 'success');
            setRunningJob(null);
            onApplied?.();
            void loadResults();
          }
        } else if (job.status === 'failed' || job.status === 'cancelled') {
          window.clearInterval(timer);
          if (cancelled) return;
          setJobError(job.error ?? (job.status === 'cancelled' ? 'Job cancelled' : 'Job failed'));
          toast.push(`Job failed${job.error ? `: ${job.error}` : ''}`, 'error');
          setRunningJob(null);
        }
      })();
    }, 3000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [runningJob, siteId, toast, loadResults, onApplied]);

  // ---- step transitions ----
  function goToMatch() {
    if (selectedPosts.size === 0) {
      toast.push('Select at least one source post', 'error');
      return;
    }
    setStep('match');
  }

  async function startMatching(forceReplan: boolean) {
    if (!forceReplan && hasExistingSuggestions) {
      // Suggestions already scored — skip re-planning, jump straight to results.
      setStep('results');
      await loadResults();
      return;
    }
    setJobError(null);
    setProgress(null);
    try {
      const { job_run_id } = await triggerPlan(siteId, topology === '' ? undefined : topology);
      setRunningJob({ id: job_run_id, type: 'plan' });
      setStep('progress');
      toast.push('Topical matching started', 'info');
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not start matching', 'error');
    }
  }

  // ---- results interactions ----
  const visibleEdges = useMemo(
    () => edges.filter((e) => scorePct(e.composite_score) >= threshold),
    [edges, threshold],
  );
  const sourceCount = useMemo(
    () => new Set(visibleEdges.map((e) => e.source_post_id)).size,
    [visibleEdges],
  );

  function toggleEdge(id: string) {
    const next = new Set(selectedEdges);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelectedEdges(next);
  }

  function toggleAllEdges() {
    const allSel = visibleEdges.length > 0 && visibleEdges.every((e) => selectedEdges.has(e.id));
    setSelectedEdges(allSel ? new Set() : new Set(visibleEdges.map((e) => e.id)));
  }

  async function decide(ids: string[], decision: 'approved' | 'rejected') {
    if (ids.length === 0 || decideBusy) return;
    setDecideBusy(true);
    try {
      await bulkDecide(ids, decision);
      toast.push(`${ids.length} match${ids.length === 1 ? '' : 'es'} ${decision}`, 'success');
      setSelectedEdges(new Set());
      await loadResults();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Decision failed', 'error');
    } finally {
      setDecideBusy(false);
    }
  }

  function approveAboveThreshold() {
    const ids = visibleEdges.filter((e) => e.status === 'suggested').map((e) => e.id);
    void decide(ids, 'approved');
  }

  // ---- export ----
  function exportCsv() {
    const header = ['source_url', 'target_url', 'anchor', 'anchor_type', 'score', 'module', 'status'];
    const lines = [header.join(',')];
    for (const e of visibleEdges) {
      lines.push(
        [
          e.source_url,
          e.target_url,
          e.anchor,
          e.anchor_type,
          String(scorePct(e.composite_score)),
          e.insert_module ?? '',
          e.status,
        ]
          .map(csvCell)
          .join(','),
      );
    }
    download(`siloforge-bulk-${siteId}.csv`, 'text/csv', lines.join('\n'));
  }

  function exportJson() {
    const rows = visibleEdges.map((e) => ({
      source_url: e.source_url,
      target_url: e.target_url,
      anchor: e.anchor,
      anchor_type: e.anchor_type,
      score: scorePct(e.composite_score),
      module: e.insert_module,
      status: e.status,
    }));
    download(`siloforge-bulk-${siteId}.json`, 'application/json', JSON.stringify(rows, null, 2));
  }

  // ---- inject ----
  async function startLinking() {
    const approvedIds = edges.filter((e) => e.status === 'approved').map((e) => e.id);
    if (approvedIds.length === 0) {
      toast.push('Approve at least one match first', 'error');
      return;
    }
    setJobError(null);
    setProgress(null);
    setAppliedCount(null);
    try {
      const { job_run_id, count } = await bulkRun(siteId, approvedIds);
      setRunningJob({ id: job_run_id, type: 'inject' });
      setStep('link');
      toast.push(`Injecting ${count} approved link${count === 1 ? '' : 's'}…`, 'info');
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not start linking', 'error');
    }
  }

  const approvedCount = useMemo(() => edges.filter((e) => e.status === 'approved').length, [edges]);
  const injecting = runningJob?.type === 'inject';

  return (
    <div className="wiz card">
      <div className="wiz-head">
        <div>
          <div className="wiz-kicker">New Bulk</div>
          <h2 className="wiz-title">
            {step === 'fetch' && 'Fetch content'}
            {step === 'match' && 'Auto-match by topical map'}
            {step === 'progress' && 'Matching in progress'}
            {step === 'results' && 'Which post links to which'}
            {step === 'link' && 'Start linking'}
          </h2>
        </div>
        <div className="spacer" />
        <button className="btn btn-ghost" onClick={onClose} aria-label="Close wizard">
          ✕ Close
        </button>
      </div>

      <Stepper current={step} />

      {/* ---------- STEP 1: FETCH ---------- */}
      {step === 'fetch' && (
        <div className="wiz-body">
          <p className="wiz-lead">
            Pick the source posts to build links <b>from</b>. Scope by topical cluster (your topical
            map), post type, or search — or import a URL list.
          </p>

          <div className="wiz-scope">
            <label className="field">
              Scope
              <select
                className="input"
                value={scope}
                onChange={(e) => setScope(e.target.value as ScopeMode)}
              >
                <option value="all">All posts</option>
                <option value="cluster">By topical cluster</option>
                <option value="post_type">By post type</option>
                <option value="search">Search title / URL</option>
              </select>
            </label>

            {scope === 'cluster' && (
              <label className="field">
                Topical cluster
                <select
                  className="input"
                  value={clusterId}
                  onChange={(e) => setClusterId(e.target.value)}
                >
                  <option value="">All clusters</option>
                  {clusters.map((c) => (
                    <option key={c.id} value={c.id}>
                      {c.label} ({c.count})
                    </option>
                  ))}
                </select>
              </label>
            )}

            {scope === 'post_type' && (
              <label className="field">
                Post type
                <select
                  className="input"
                  value={postType}
                  onChange={(e) => setPostType(e.target.value)}
                >
                  <option value="">All types</option>
                  {postTypes.map((t) => (
                    <option key={t} value={t}>
                      {t}
                    </option>
                  ))}
                </select>
              </label>
            )}

            {scope === 'search' && (
              <label className="field">
                Search
                <input
                  type="text"
                  className="input"
                  placeholder="title or URL…"
                  value={search}
                  onChange={(e) => setSearch(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') void runFetch();
                  }}
                />
              </label>
            )}

            <label className="field wiz-orphan">
              <span>&nbsp;</span>
              <span className="dup-toggle">
                <input
                  type="checkbox"
                  checked={orphansOnly}
                  onChange={(e) => setOrphansOnly(e.target.checked)}
                />
                Orphans only
              </span>
            </label>

            <button
              className="btn btn-primary wiz-fetch-btn"
              disabled={postsLoading}
              onClick={() => void runFetch()}
              data-hint="Loads the posts matching the scope above into the table below so you can pick sources. Reads content only — nothing is changed."
            >
              {postsLoading ? 'Fetching…' : 'Fetch'}
            </button>
          </div>

          <details className="wiz-import">
            <summary>Import URL list (filter fetched posts)</summary>
            <div className="wiz-import-body">
              <textarea
                className="input wiz-textarea"
                placeholder="Paste URLs — one per line or comma-separated (CSV / JSON also accepted)…"
                value={importText}
                onChange={(e) => setImportText(e.target.value)}
                rows={4}
              />
              <div className="wiz-import-actions">
                <button className="btn btn-sm" onClick={() => fileRef.current?.click()}>
                  Upload .csv / .json
                </button>
                <input
                  ref={fileRef}
                  type="file"
                  accept=".csv,.json,text/csv,application/json"
                  style={{ display: 'none' }}
                  onChange={(e) => void onImportFile(e)}
                />
                <button className="btn btn-sm btn-primary" onClick={() => applyImport(importText)}>
                  Apply URL filter
                </button>
                {importSet && (
                  <span className="edge-filter-count">
                    Filter active · {importSet.size} URL{importSet.size === 1 ? '' : 's'}
                    <button
                      className="btn-ghost btn-sm"
                      onClick={() => {
                        setImportSet(null);
                        setImportText('');
                      }}
                    >
                      Clear
                    </button>
                  </span>
                )}
              </div>
            </div>
          </details>

          {postsError && <div className="error-box">{postsError}</div>}

          <div className="table-toolbar">
            <span className="muted">
              {postsLoading
                ? 'Loading…'
                : `${visiblePosts.length}${
                    importSet ? ` of ${posts.length} (URL-filtered)` : ''
                  } post${visiblePosts.length === 1 ? '' : 's'} · ${postTotal} total`}
            </span>
            <div className="spacer" />
            <span className="sel-info">{selectedPosts.size} selected</span>
            <button
              className="btn btn-primary"
              disabled={selectedPosts.size === 0}
              onClick={goToMatch}
              data-hint="Move to the matching step with the posts you've ticked as link sources."
            >
              Submit / Start matching →
            </button>
          </div>

          {postsLoading ? (
            <Skeleton rows={8} />
          ) : visiblePosts.length === 0 ? (
            <EmptyState
              title={fetched ? 'No posts match these filters' : 'No posts yet'}
              hint="Adjust the scope or clear the URL filter, then Fetch."
            />
          ) : (
            <div className="tbl-wrap">
              <table className="tbl">
                <thead>
                  <tr>
                    <th style={{ width: 34 }}>
                      <input
                        type="checkbox"
                        checked={allPostsSelected}
                        onChange={toggleAllPosts}
                        aria-label="Select all posts"
                      />
                    </th>
                    <th>Title</th>
                    <th>URL</th>
                    <th>Cluster</th>
                    <th>Type</th>
                    <th>Orphan</th>
                    <th className="num">Suggestions</th>
                  </tr>
                </thead>
                <tbody>
                  {visiblePosts.map((p) => (
                    <tr key={p.id}>
                      <td>
                        <input
                          type="checkbox"
                          checked={selectedPosts.has(p.id)}
                          onChange={() => togglePost(p.id)}
                          aria-label="Select post"
                        />
                      </td>
                      <td>
                        <a href={p.url} target="_blank" rel="noopener noreferrer">
                          {p.title || '(untitled)'}
                        </a>
                      </td>
                      <td className="mono-sm" title={p.url}>
                        {p.url}
                      </td>
                      <td className="muted">{p.cluster_label ?? '—'}</td>
                      <td>
                        <span className="chip chip-mode">{p.post_type}</span>
                      </td>
                      <td>
                        {p.is_orphan ? (
                          <span className="chip chip-rejected">orphan</span>
                        ) : (
                          <span className="faint">—</span>
                        )}
                      </td>
                      <td className="num">{p.suggested_out}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}

      {/* ---------- STEP 2: MATCH ---------- */}
      {step === 'match' && (
        <div className="wiz-body">
          <div className="card wiz-match">
            <div className="wiz-match-big">{selectedPosts.size}</div>
            <div className="wiz-match-sub">source post{selectedPosts.size === 1 ? '' : 's'} selected</div>
            <p className="wiz-lead" style={{ textAlign: 'center', marginTop: 14 }}>
              Matching uses <b>semantic NLP</b> (paragraph embeddings) + your <b>topical map</b>{' '}
              (clusters) across the 11-dimension relevancy model. The composite score <i>is</i> the
              topical relevancy.
            </p>

            {hasExistingSuggestions ? (
              <div className="info-note" role="note" style={{ marginTop: 8 }}>
                <span className="info-note-icon" aria-hidden="true">
                  ✓
                </span>
                <span className="info-note-text">
                  <b>{selectedSuggestedOut}</b> suggestion
                  {selectedSuggestedOut === 1 ? '' : 's'} already scored for these posts — no need to
                  re-run.
                </span>
              </div>
            ) : (
              <div className="info-note" role="note" style={{ marginTop: 8 }}>
                <span className="info-note-icon" aria-hidden="true">
                  ℹ
                </span>
                <span className="info-note-text">
                  No suggestions scored yet for these posts — matching will run now.
                </span>
              </div>
            )}

            <label className="field wiz-topology">
              Silo topology
              <select
                className="input"
                value={topology}
                onChange={(e) => setTopology(e.target.value as SiloTopology | '')}
                data-hint="Optional shape for the links this match builds. Leave on “site default” to use whatever the site is configured for."
              >
                <option value="">site default</option>
                {SILO_TOPOLOGIES.map((t) => (
                  <option key={t} value={t}>
                    {t} — {TOPOLOGY_HINTS[t]}
                  </option>
                ))}
              </select>
            </label>

            <div className="wiz-match-actions">
              <button className="btn" onClick={() => setStep('fetch')}>
                ← Back
              </button>
              <div className="spacer" />
              {hasExistingSuggestions && (
                <button className="btn" onClick={() => void startMatching(true)}>
                  Re-run topical match
                </button>
              )}
              <button
                className="btn btn-primary"
                onClick={() => void startMatching(false)}
                data-hint="Runs the NLP topical match (semantic embeddings + your topical map) to score link candidates for the selected posts, or jumps straight to results if they're already scored."
              >
                {hasExistingSuggestions ? 'View results →' : 'Start matching →'}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* ---------- STEP 3: PROGRESS ---------- */}
      {step === 'progress' && (
        <div className="wiz-body">
          <ProgressPanel progress={progress} title="Topical matching" />
          {jobError && (
            <>
              <div className="error-box" style={{ marginTop: 14 }}>
                {jobError}
              </div>
              <div className="table-toolbar" style={{ marginTop: 12 }}>
                <button className="btn" onClick={() => setStep('match')}>
                  ← Back
                </button>
                <button className="btn btn-primary" onClick={() => void startMatching(true)}>
                  Retry
                </button>
              </div>
            </>
          )}
        </div>
      )}

      {/* ---------- STEP 4: RESULTS ---------- */}
      {step === 'results' && (
        <div className="wiz-body">
          {edgesError && <div className="error-box">{edgesError}</div>}

          <div className="wiz-results-head">
            <div>
              <div className="wiz-results-count">
                {visibleEdges.length} link{visibleEdges.length === 1 ? '' : 's'} matched across{' '}
                {sourceCount} source post{sourceCount === 1 ? '' : 's'}
              </div>
              <div className="faint" style={{ fontSize: 12.5 }}>
                Click any source or target to open the post in a new tab.
              </div>
            </div>
            <div className="spacer" />
            <button
              className="btn btn-sm"
              onClick={exportCsv}
              disabled={visibleEdges.length === 0}
              data-hint="Download the matched links currently shown (above the relevancy threshold) as a CSV file."
            >
              Export CSV
            </button>
            <button
              className="btn btn-sm"
              onClick={exportJson}
              disabled={visibleEdges.length === 0}
              data-hint="Download the matched links currently shown (above the relevancy threshold) as a JSON file."
            >
              Export JSON
            </button>
            <button className="btn btn-sm" onClick={() => void startMatching(true)}>
              Re-run topical match
            </button>
          </div>

          <div className="wiz-results-controls">
            <label className="wiz-threshold">
              Min relevancy: <b>{threshold}</b>
              <input
                type="range"
                min={0}
                max={100}
                value={threshold}
                onChange={(e) => setThreshold(Number(e.target.value))}
              />
            </label>
            <div className="spacer" />
            {selectedEdges.size > 0 && (
              <>
                <span className="sel-info">{selectedEdges.size} selected</span>
                <button
                  className="btn btn-sm btn-primary"
                  disabled={decideBusy}
                  onClick={() => void decide([...selectedEdges], 'approved')}
                >
                  Approve selected
                </button>
                <button
                  className="btn btn-sm btn-danger"
                  disabled={decideBusy}
                  onClick={() => void decide([...selectedEdges], 'rejected')}
                >
                  Reject selected
                </button>
              </>
            )}
            <button
              className="btn btn-sm"
              disabled={decideBusy || visibleEdges.length === 0}
              onClick={approveAboveThreshold}
              data-hint="Approve every suggestion at or above the current minimum-relevancy slider value in one click. Still injected only when you Start Linking."
            >
              Approve all ≥ {threshold}
            </button>
          </div>

          {edgesLoading ? (
            <Skeleton rows={8} />
          ) : visibleEdges.length === 0 ? (
            <EmptyState
              title="No matches at this threshold"
              hint="Lower the minimum relevancy, or re-run the topical match."
            />
          ) : (
            <ResultsTable
              edges={visibleEdges}
              selected={selectedEdges}
              onToggle={toggleEdge}
              onToggleAll={toggleAllEdges}
              onDecide={(ids, d) => void decide(ids, d)}
              busy={decideBusy}
            />
          )}

          <div className="wiz-foot">
            <button className="btn" onClick={() => setStep('match')}>
              ← Back
            </button>
            <div className="spacer" />
            <span className="muted" style={{ alignSelf: 'center' }}>
              {approvedCount} approved
            </span>
            <button
              className="btn btn-primary"
              disabled={approvedCount === 0}
              onClick={() => void startLinking()}
              data-hint="Injects the approved links into your live content now. This is the step that actually edits your pages."
            >
              Start Linking (inject {approvedCount}) →
            </button>
          </div>
        </div>
      )}

      {/* ---------- STEP 5: LINK ---------- */}
      {step === 'link' && (
        <div className="wiz-body">
          {injecting ? (
            <ProgressPanel progress={progress} title="Applying links" />
          ) : (
            <>
              {appliedCount !== null && (
                <div className="info-note" role="note">
                  <span className="info-note-icon" aria-hidden="true">
                    ✓
                  </span>
                  <span className="info-note-text">
                    <b>{appliedCount}</b> link{appliedCount === 1 ? '' : 's'} applied.
                  </span>
                </div>
              )}
              {jobError && <div className="error-box">{jobError}</div>}

              <ResultsTable
                edges={edges}
                selected={selectedEdges}
                onToggle={toggleEdge}
                onToggleAll={toggleAllEdges}
                onDecide={(ids, d) => void decide(ids, d)}
                busy={decideBusy}
              />

              <div className="wiz-foot">
                <button className="btn" onClick={() => setStep('results')}>
                  ← Back to results
                </button>
                <div className="spacer" />
                <button className="btn btn-sm" onClick={exportCsv}>
                  Export CSV
                </button>
                <button className="btn btn-sm" onClick={exportJson}>
                  Export JSON
                </button>
                <button className="btn btn-primary" onClick={onClose}>
                  Done
                </button>
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
