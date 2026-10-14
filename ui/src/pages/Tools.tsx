import { useCallback, useEffect, useMemo, useState } from 'react';
import { useParams, useSearchParams } from 'react-router-dom';
import {
  bulkRun,
  bulkUndo,
  EDGE_PAGE_SIZE,
  fetchAllEdges,
  fetchEdges,
  fetchSite,
  patchSiteSettings,
  triggerArchivePlan,
  type EdgeQuery,
} from '../api';
import { BulkModuleInfo, BulkSafetyNote } from '../components/BulkGuidance';
import { BulkWizard } from '../components/BulkWizard';
import { CustomClustersPanel } from '../components/CustomClustersPanel';
import { EdgeTable } from '../components/EdgeTable';
import { IgnoreKeywordsPanel, KeywordLinkerPanel } from '../components/RulesPanels';
import { EmptyState } from '../components/Skeleton';
import { useJobTracker } from '../hooks';
import { useToast } from '../toast';
import {
  ANCHOR_TYPES,
  ANCHOR_TYPE_HINTS,
  type AnchorType,
  type EdgeView,
  type InsertModule,
} from '../types';

type ToolTab = 'inbound' | 'outbound' | 'silo' | 'archive' | 'bulk' | 'anchors' | 'keyword_linker' | 'ignore';

const TABS: Array<{ id: ToolTab; label: string; group: 'build' | 'rules'; hint: string }> = [
  {
    id: 'inbound',
    label: 'Inbound',
    group: 'build',
    hint: 'Candidate links grouped by the page they point TO — use this to give orphan / weak pages more incoming links.',
  },
  {
    id: 'outbound',
    label: 'Outbound',
    group: 'build',
    hint: 'Candidate links grouped by the page they come FROM — use this to give dead-end pages outgoing links.',
  },
  {
    id: 'silo',
    label: 'Silo',
    group: 'build',
    hint: 'Suggestions that follow your silo topology (reverse / serial / priority / circle / bidirectional), keeping link equity inside topical clusters.',
  },
  {
    id: 'archive',
    label: 'Archive Silo',
    group: 'build',
    hint: 'Suggestions that link posts to their WordPress category / tag archive pages.',
  },
  {
    id: 'bulk',
    label: 'Bulk',
    group: 'build',
    hint: 'The queue of approved links plus the guided New Bulk wizard — this is where links actually get injected.',
  },
  {
    id: 'anchors',
    label: 'Anchors',
    group: 'build',
    hint: 'Review and edit the anchor text and anchor type (exact / partial / branded / generic / LSI / naked URL / CTA) for each suggestion.',
  },
  {
    id: 'keyword_linker',
    label: 'Keyword Linker',
    group: 'rules',
    hint: 'Rules that auto-link a keyword to a target page wherever that phrase appears in your content.',
  },
  {
    id: 'ignore',
    label: 'Ignore Keywords',
    group: 'rules',
    hint: 'A stop-list of phrases the linker must never turn into anchor text (brand safety).',
  },
];

const EDGE_TABS: ToolTab[] = ['inbound', 'outbound', 'silo', 'archive', 'bulk', 'anchors'];
// Candidate views that get the client-side source/target filter box.
const FILTER_TABS: ToolTab[] = ['inbound', 'outbound', 'bulk'];
const SINGLE_MODES: Array<InsertModule | 'pipeline'> = ['pipeline', 'native', 'rephrase', 'context'];
const PRESETS = ['conservative', 'balanced', 'aggressive'] as const;

const TYPE_COLORS: Record<AnchorType, string> = {
  exact: '#e11d48',
  partial: '#0d9488',
  branded: '#2f6bff',
  generic: '#64748b',
  lsi: '#9333ea',
  naked: '#0891b2',
  cta: '#d97706',
};

function AnchorTypeDistribution({ edges }: { edges: EdgeView[] }) {
  const counts = useMemo(() => {
    const m = new Map<AnchorType, number>();
    for (const e of edges) m.set(e.anchor_type, (m.get(e.anchor_type) ?? 0) + 1);
    return m;
  }, [edges]);
  const total = edges.length;
  if (total === 0) return null;
  return (
    <div className="card dist-card">
      <div className="card-title">Anchor type distribution</div>
      {ANCHOR_TYPES.map((t) => {
        const n = counts.get(t) ?? 0;
        const pct = total ? (n / total) * 100 : 0;
        return (
          <div className="dist-row" key={t} data-hint={`${t} — ${ANCHOR_TYPE_HINTS[t]}`}>
            <span className="dist-label">{t}</span>
            <span className="dist-bar">
              <span className="dist-fill" style={{ width: `${pct}%`, background: TYPE_COLORS[t] }} />
            </span>
            <span className="dist-count">
              {n} · {pct.toFixed(0)}%
            </span>
          </div>
        );
      })}
    </div>
  );
}

const TOOL_TABS: ToolTab[] = ['inbound', 'outbound', 'silo', 'archive', 'bulk', 'anchors', 'keyword_linker', 'ignore'];

export function ToolsPage() {
  const { siteId } = useParams<{ siteId: string }>();
  const [searchParams, setSearchParams] = useSearchParams();
  const toast = useToast();
  const track = useJobTracker();

  const requestedTab = searchParams.get('tab');
  const initialTab: ToolTab = TOOL_TABS.includes(requestedTab as ToolTab) ? (requestedTab as ToolTab) : 'inbound';
  const [tab, setTab] = useState<ToolTab>(initialTab);
  const [edges, setEdges] = useState<EdgeView[]>([]);
  const [total, setTotal] = useState(0);
  // Rows fetched from the server so far (before client-side silo filtering) = next offset.
  const [fetched, setFetched] = useState(0);
  const [loadingMore, setLoadingMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [singleMode, setSingleMode] = useState<InsertModule | 'pipeline'>('pipeline');
  const [preset, setPreset] = useState<'conservative' | 'balanced' | 'aggressive'>('balanced');
  const [filter, setFilter] = useState('');
  const [wizardOpen, setWizardOpen] = useState(searchParams.get('bulk') === '1');
  // Dashboard "Add link" deep-links here with ?target=<post_id> to focus one orphan's inbound candidates.
  const targetId = searchParams.get('target');
  const targetActive = tab === 'inbound' && !!targetId;

  const clearTarget = useCallback(() => {
    const next = new URLSearchParams(searchParams);
    next.delete('target');
    setSearchParams(next, { replace: true });
  }, [searchParams, setSearchParams]);

  // Client-side scope filter: match the source or target post title/URL.
  // EdgeView carries no cluster mapping, so a title/URL search is the honest scope control here.
  const visibleEdges = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q || !FILTER_TABS.includes(tab)) return edges;
    return edges.filter(
      (e) =>
        e.source_title.toLowerCase().includes(q) ||
        e.target_title.toLowerCase().includes(q) ||
        e.source_url.toLowerCase().includes(q) ||
        e.target_url.toLowerCase().includes(q),
    );
  }, [edges, filter, tab]);

  const edgeQuery = useCallback((): Omit<EdgeQuery, 'limit' | 'offset'> => {
    const status = tab === 'bulk' ? 'approved' : 'suggested';
    // Archive Silo edges carry origin='archive'; filter them server-side via the tool param.
    return { status, ...(tab === 'archive' ? { tool: 'archive' } : {}) };
  }, [tab]);

  const load = useCallback(async () => {
    if (!siteId || !EDGE_TABS.includes(tab)) {
      setLoading(false);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      if (targetActive && targetId) {
        // A single target's inbound candidates can sit on any page, so walk them all.
        const res = await fetchAllEdges(siteId, edgeQuery());
        const list = res.edges.filter((e) => e.target_post_id === targetId);
        setEdges(list);
        setTotal(list.length);
        setFetched(res.total);
      } else {
        const res = await fetchEdges(siteId, { ...edgeQuery(), limit: EDGE_PAGE_SIZE, offset: 0 });
        const list = tab === 'silo' ? res.edges.filter((e) => e.silo_topology !== null) : res.edges;
        setEdges(list);
        setTotal(res.total);
        setFetched(res.edges.length);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load edges');
      setEdges([]);
      setFetched(0);
    } finally {
      setLoading(false);
    }
  }, [siteId, tab, targetActive, targetId, edgeQuery]);

  const loadMore = useCallback(async () => {
    if (!siteId || loadingMore) return;
    setLoadingMore(true);
    try {
      const res = await fetchEdges(siteId, { ...edgeQuery(), limit: EDGE_PAGE_SIZE, offset: fetched });
      const list = tab === 'silo' ? res.edges.filter((e) => e.silo_topology !== null) : res.edges;
      setEdges((cur) => {
        const seen = new Set(cur.map((e) => e.id));
        return [...cur, ...list.filter((e) => !seen.has(e.id))];
      });
      setTotal(res.total);
      setFetched((n) => n + res.edges.length);
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Failed to load more edges', 'error');
    } finally {
      setLoadingMore(false);
    }
  }, [siteId, loadingMore, edgeQuery, fetched, tab, toast]);

  useEffect(() => {
    void load();
  }, [load]);

  // Reset the scope filter whenever the active tab changes.
  useEffect(() => {
    setFilter('');
  }, [tab]);

  // Load site settings once so the Bulk tab's mode/preset selectors reflect current config.
  useEffect(() => {
    if (!siteId) return;
    void fetchSite(siteId)
      .then((s) => {
        const mods = s.settings.insert_modules;
        setSingleMode(mods && mods.length === 1 ? mods[0] : 'pipeline');
        setPreset(s.settings.preset ?? 'balanced');
      })
      .catch(() => undefined);
  }, [siteId]);

  async function saveBulkSettings(mode: InsertModule | 'pipeline', p: 'conservative' | 'balanced' | 'aggressive') {
    if (!siteId) return;
    setSingleMode(mode);
    setPreset(p);
    const insert_modules =
      mode === 'pipeline' ? (['native', 'rephrase', 'context'] as InsertModule[]) : [mode];
    try {
      await patchSiteSettings(siteId, { insert_modules, preset: p });
      toast.push('Bulk settings saved', 'success');
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not save settings', 'error');
    }
  }

  async function runBulkInject() {
    if (!siteId || busy) return;
    setBusy(true);
    try {
      const { job_run_id, count } = await bulkRun(siteId);
      toast.push(`Injecting ${count} approved edge${count === 1 ? '' : 's'}…`, 'info');
      track(siteId, job_run_id, 'Bulk inject', () => void load());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Bulk inject failed to start', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function buildArchiveSilo() {
    if (!siteId || busy) return;
    setBusy(true);
    try {
      const { job_run_id } = await triggerArchivePlan(siteId);
      toast.push('Building archive silo…', 'info');
      track(siteId, job_run_id, 'Archive silo plan', () => void load());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Archive silo failed to start', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function runUndoAll() {
    if (!siteId || busy) return;
    setBusy(true);
    try {
      const { job_run_id, count } = await bulkUndo(siteId);
      toast.push(`Reverting ${count} applied edge${count === 1 ? '' : 's'}…`, 'info');
      track(siteId, job_run_id, 'Bulk undo', () => void load());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Undo failed to start', 'error');
    } finally {
      setBusy(false);
    }
  }

  if (!siteId) return <EmptyState title="No site selected" hint="Pick a site from the selector above." />;

  const tableProps = (() => {
    switch (tab) {
      case 'inbound':
        return {
          groupBy: 'target' as const,
          emptyTitle: 'No inbound suggestions yet',
          emptyHint: 'Run a Plan to generate candidate links.',
        };
      case 'outbound':
        return {
          groupBy: 'source' as const,
          emptyTitle: 'No outbound suggestions yet',
          emptyHint: 'Run a Plan to generate candidate links.',
        };
      case 'silo':
        return {
          emptyTitle: 'No silo suggestions yet',
          emptyHint:
            'Run a Plan with a topology (reverse / serial / priority / circle / bidirectional) to build silo edges.',
        };
      case 'archive':
        return {
          emptyTitle: 'No archive silo suggestions yet',
          emptyHint: 'Crawl a WordPress site (categories/tags become archive pages), then Build Archive Silo.',
        };
      case 'anchors':
        return {
          anchorProminent: true,
          emptyTitle: 'No anchors to review yet',
          emptyHint: 'Run a Plan to generate anchored suggestions.',
        };
      case 'bulk':
        return {
          allowDecide: false,
          emptyTitle: 'No approved edges',
          emptyHint: 'Approve suggestions in the other tabs, then inject them from here.',
        };
    }
  })();

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Tools</h1>
          <div className="page-sub">Review, approve and inject candidate internal links.</div>
        </div>
      </div>

      <div className="tabs">
        {TABS.map((t, i) => (
          <span key={t.id} style={{ display: 'contents' }}>
            {i > 0 && TABS[i - 1].group !== t.group && <span className="tab-group-sep" title="Rules" />}
            <button className={`tab${tab === t.id ? ' active' : ''}`} onClick={() => setTab(t.id)} data-hint={t.hint}>
              {t.label}
              {tab === t.id && !loading && EDGE_TABS.includes(t.id) && <span className="count">{edges.length}</span>}
            </button>
          </span>
        ))}
      </div>

      {error && <div className="error-box" style={{ marginBottom: 14 }}>{error}</div>}

      {targetActive && (
        <div className="table-toolbar" style={{ marginBottom: 14 }}>
          <span className="muted">
            Showing inbound candidates for one page
            {edges[0] ? (
              <>
                : <b>{edges[0].target_title || edges[0].target_url}</b>
              </>
            ) : (
              ''
            )}
            . {loading ? '' : edges.length === 0 ? 'No suggestions point to it yet — run a Plan.' : ''}
          </span>
          <div className="spacer" />
          <button className="btn btn-sm" onClick={clearTarget} data-hint="Show inbound candidates for every page again.">
            Show all pages
          </button>
        </div>
      )}

      {tab === 'keyword_linker' && <KeywordLinkerPanel siteId={siteId} />}
      {tab === 'ignore' && <IgnoreKeywordsPanel siteId={siteId} />}

      {tab === 'silo' && <CustomClustersPanel siteId={siteId} />}

      {tab === 'archive' && (
        <div className="table-toolbar" style={{ marginBottom: 14 }}>
          <span className="muted">
            {loading ? 'Loading…' : `${edges.length} archive-origin suggestion${edges.length === 1 ? '' : 's'}`}
          </span>
          <div className="spacer" />
          <button
            className="btn btn-primary"
            disabled={busy}
            onClick={() => void buildArchiveSilo()}
            data-hint="Generates link suggestions that connect posts to their category/tag archive pages. Creates suggestions only — nothing is applied yet."
          >
            Build Archive Silo
          </button>
        </div>
      )}

      {tab === 'bulk' && wizardOpen && (
        <BulkWizard siteId={siteId} onClose={() => setWizardOpen(false)} onApplied={() => void load()} />
      )}

      {tab === 'bulk' && !wizardOpen && (
        <>
          <div className="table-toolbar" style={{ marginBottom: 14 }}>
            <span className="muted">
              Guided flow: fetch source posts → auto-match by topical map → review → inject.
            </span>
            <div className="spacer" />
            <button
              className="btn btn-primary"
              onClick={() => setWizardOpen(true)}
              data-hint="Opens the guided bulk wizard: fetch source posts → auto-match by topical map → review → inject."
            >
              ＋ New Bulk
            </button>
          </div>
          <div className="card" style={{ marginBottom: 14 }}>
            <div className="card-title">Insertion settings</div>
            <div className="bulk-settings">
              <label data-hint="How the link is inserted: Pipeline tries native → rephrase → context in order; a Single mode forces just that one method. Saved to the site immediately.">
                Mode
                <select className="input" value={singleMode} onChange={(e) => void saveBulkSettings(e.target.value as InsertModule | 'pipeline', preset)}>
                  {SINGLE_MODES.map((m) => (
                    <option key={m} value={m}>
                      {m === 'pipeline' ? 'Pipeline (native → rephrase → context)' : `Single: ${m}`}
                    </option>
                  ))}
                </select>
              </label>
              <label data-hint="How many links to place and how strict the relevancy bar is: conservative = fewer, safer links; aggressive = more links, lower threshold. Saved to the site immediately.">
                Preset
                <select className="input" value={preset} onChange={(e) => void saveBulkSettings(singleMode, e.target.value as 'conservative' | 'balanced' | 'aggressive')}>
                  {PRESETS.map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))}
                </select>
              </label>
            </div>
          </div>
          <BulkModuleInfo mode={singleMode} />
          <BulkSafetyNote />
          <div className="table-toolbar">
            <span className="muted">
              {loading ? 'Loading…' : `${edges.length} approved edge${edges.length === 1 ? '' : 's'} ready to inject`}
            </span>
            <div className="spacer" />
            <button
              className="btn btn-primary"
              disabled={busy || loading || edges.length === 0}
              onClick={() => void runBulkInject()}
              data-hint="Writes every approved link into your live content. This is the step that actually edits your pages."
            >
              Run Bulk Inject
            </button>
            <button
              className="btn btn-danger"
              disabled={busy}
              onClick={() => void runUndoAll()}
              data-hint="Removes every internal link this tool has applied to the site, reverting the content changes. Destructive — use with care."
            >
              Undo All
            </button>
          </div>
        </>
      )}

      {tab === 'anchors' && !loading && <AnchorTypeDistribution edges={edges} />}

      {!(tab === 'bulk' && wizardOpen) && FILTER_TABS.includes(tab) && !loading && edges.length > 0 && (
        <div className="edge-filter">
          <input
            type="text"
            className="input"
            placeholder="Filter by source / target title or URL…"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            aria-label="Filter candidates by source or target"
          />
          {filter.trim() !== '' && (
            <span className="edge-filter-count">
              {visibleEdges.length} of {edges.length} match
              <button className="btn-ghost btn-sm" onClick={() => setFilter('')}>
                Clear
              </button>
            </span>
          )}
        </div>
      )}

      {EDGE_TABS.includes(tab) && !(tab === 'bulk' && wizardOpen) && (
        <EdgeTable
          edges={visibleEdges}
          loading={loading}
          onChanged={() => void load()}
          allowAnchorEdit={tab === 'anchors'}
          {...tableProps}
        />
      )}

      {!loading && EDGE_TABS.includes(tab) && !(tab === 'bulk' && wizardOpen) && !targetActive && fetched < total && (
        <div className="table-toolbar" style={{ marginTop: 10 }}>
          <span className="muted" style={{ fontSize: 12.5 }}>
            Loaded {fetched} of {total}.
          </span>
          <button
            className="btn btn-sm"
            disabled={loadingMore}
            onClick={() => void loadMore()}
            data-hint={`Fetch the next ${EDGE_PAGE_SIZE} candidates from the server.`}
          >
            {loadingMore ? 'Loading…' : 'Load more'}
          </button>
        </div>
      )}
    </div>
  );
}
