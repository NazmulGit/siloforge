import { useCallback, useEffect, useMemo, useState, type CSSProperties } from 'react';
import { useParams } from 'react-router-dom';
import ReactFlow, { Background, Controls, type Edge, type Node } from 'reactflow';
import { bulkDecide, fetchAllEdges, fetchGraphReport, fetchLinksReport } from '../api';
import { PostLink, UrlMono } from '../components/PostLink';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { scoreTone } from '../format';
import { useToast } from '../toast';
import type { GraphReport, GraphReportNode, LinkReportRow } from '../types';

type ReportTab = 'links' | 'visualizer';

// ---------------- Links tab ----------------

type NumKey = 'inbound' | 'outbound' | 'pagerank' | 'click_depth' | 'link_flow_score';
type SortKey = 'title' | 'url' | NumKey;

const NUM_COLS: Array<{ key: NumKey; label: string }> = [
  { key: 'inbound', label: 'Inbound' },
  { key: 'outbound', label: 'Outbound' },
  { key: 'pagerank', label: 'PageRank' },
  { key: 'click_depth', label: 'Depth' },
  { key: 'link_flow_score', label: 'Link Flow' },
];

interface MinMax {
  min: string;
  max: string;
}

function LinksTab({ siteId }: { siteId: string }) {
  const toast = useToast();
  const [rows, setRows] = useState<LinkReportRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [sort, setSort] = useState<{ key: SortKey; dir: 1 | -1 }>({ key: 'link_flow_score', dir: -1 });
  const [filters, setFilters] = useState<Partial<Record<NumKey, MinMax>>>({});
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [sending, setSending] = useState(false);
  const [dupOnly, setDupOnly] = useState(false);

  useEffect(() => {
    let alive = true;
    setRows(null);
    setSelected(new Set());
    fetchLinksReport(siteId)
      .then((r) => {
        if (alive) setRows(r);
      })
      .catch((e: unknown) => {
        if (alive) setError(e instanceof Error ? e.message : 'Failed to load report');
      });
    return () => {
      alive = false;
    };
  }, [siteId]);

  const visible = useMemo(() => {
    let list = rows ?? [];
    if (dupOnly) list = list.filter((r) => (r.duplicate_count ?? 0) > 0);
    for (const col of NUM_COLS) {
      const f = filters[col.key];
      if (!f) continue;
      const min = f.min.trim() === '' ? null : Number(f.min);
      const max = f.max.trim() === '' ? null : Number(f.max);
      if (min !== null && !Number.isNaN(min)) list = list.filter((r) => r[col.key] >= min);
      if (max !== null && !Number.isNaN(max)) list = list.filter((r) => r[col.key] <= max);
    }
    const { key, dir } = sort;
    return [...list].sort((a, b) => {
      if (key === 'title' || key === 'url') return a[key].localeCompare(b[key]) * dir;
      return (a[key] - b[key]) * dir;
    });
  }, [rows, filters, sort, dupOnly]);

  const dupTotal = useMemo(() => (rows ?? []).filter((r) => (r.duplicate_count ?? 0) > 0).length, [rows]);

  function clickSort(key: SortKey) {
    setSort((cur) => (cur.key === key ? { key, dir: cur.dir === 1 ? -1 : 1 } : { key, dir: -1 }));
  }

  function setFilter(key: NumKey, side: 'min' | 'max', value: string) {
    setFilters((cur) => ({ ...cur, [key]: { min: '', max: '', ...cur[key], [side]: value } }));
  }

  function toggleRow(id: string) {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelected(next);
  }

  const allSelected = visible.length > 0 && visible.every((r) => selected.has(r.post_id));

  async function approvePostsToBulk(postIds: ReadonlySet<string>, clearSelection: boolean) {
    if (postIds.size === 0 || sending) return;
    setSending(true);
    try {
      const { edges } = await fetchAllEdges(siteId, { status: 'suggested' });
      const ids = edges
        .filter((e) => postIds.has(e.source_post_id) || postIds.has(e.target_post_id))
        .map((e) => e.id);
      if (ids.length === 0) {
        toast.push('No suggested edges found for those posts — run a Plan first.', 'info');
        return;
      }
      const { updated } = await bulkDecide(ids, 'approved');
      toast.push(`${updated} edge${updated === 1 ? '' : 's'} approved — see Tools → Bulk.`, 'success');
      if (clearSelection) setSelected(new Set());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Failed to send to Bulk', 'error');
    } finally {
      setSending(false);
    }
  }

  function sendToBulk() {
    void approvePostsToBulk(selected, true);
  }

  function exportFilteredToBulk() {
    void approvePostsToBulk(new Set(visible.map((r) => r.post_id)), false);
  }

  if (error) return <div className="error-box">{error}</div>;
  if (rows === null) return <Skeleton rows={8} />;
  if (rows.length === 0) return <EmptyState title="No link data yet" hint="Run a Crawl and a Plan to build the link report." />;

  const arrow = (key: SortKey) => (sort.key === key ? (sort.dir === -1 ? ' ↓' : ' ↑') : '');

  return (
    <div>
      <div className="table-toolbar">
        <span className="muted">
          {visible.length} of {rows.length} posts
        </span>
        <label
          className="dup-toggle"
          title="Show only posts involved in duplicate link edges"
          data-hint="Filter the table down to only posts that are touched by duplicate link edges, so you can find and trim repeats."
        >
          <input type="checkbox" checked={dupOnly} onChange={(e) => setDupOnly(e.target.checked)} />
          Show duplicate links only
          {dupTotal > 0 && <span className="chip chip-dupe">{dupTotal}</span>}
        </label>
        <div className="spacer" />
        <button
          className="btn btn-sm"
          disabled={sending || visible.length === 0}
          onClick={exportFilteredToBulk}
          title="Approve the suggested links for every post matching the current Min/Max filters and route them to Tools → Bulk"
          data-hint="Approves the suggested links for every post currently matching your Min/Max filters and sends them to Tools → Bulk for injection."
        >
          {sending ? 'Sending…' : `Export filtered to Bulk (${visible.length})`}
        </button>
        {selected.size > 0 && (
          <>
            <span className="sel-info">{selected.size} selected</span>
            <button
              className="btn btn-sm btn-primary"
              disabled={sending}
              onClick={sendToBulk}
              data-hint="Approves the suggested links for the posts you've ticked and routes them to Tools → Bulk for injection."
            >
              {sending ? 'Sending…' : 'Send selection to Bulk'}
            </button>
          </>
        )}
      </div>
      <div className="tbl-wrap">
        <table className="tbl">
          <thead>
            <tr>
              <th style={{ width: 34 }}>
                <input
                  type="checkbox"
                  checked={allSelected}
                  onChange={() => setSelected(allSelected ? new Set() : new Set(visible.map((r) => r.post_id)))}
                  aria-label="Select all"
                />
              </th>
              <th className="sortable" onClick={() => clickSort('title')}>
                Title{arrow('title')}
              </th>
              <th className="sortable" onClick={() => clickSort('url')}>
                URL{arrow('url')}
              </th>
              {NUM_COLS.map((c) => (
                <th key={c.key} className="num sortable" onClick={() => clickSort(c.key)}>
                  {c.label}
                  {arrow(c.key)}
                </th>
              ))}
            </tr>
            <tr className="filters-row">
              <td />
              <td className="faint" data-hint="Type a minimum and/or maximum in the boxes under any metric column to narrow the table to posts within that range.">min / max filters →</td>
              <td />
              {NUM_COLS.map((c) => (
                <td key={c.key}>
                  <div className="minmax">
                    <input
                      type="text"
                      placeholder="min"
                      value={filters[c.key]?.min ?? ''}
                      onChange={(e) => setFilter(c.key, 'min', e.target.value)}
                      aria-label={`${c.label} min`}
                    />
                    <input
                      type="text"
                      placeholder="max"
                      value={filters[c.key]?.max ?? ''}
                      onChange={(e) => setFilter(c.key, 'max', e.target.value)}
                      aria-label={`${c.label} max`}
                    />
                  </div>
                </td>
              ))}
            </tr>
          </thead>
          <tbody>
            {visible.map((r) => (
              <tr key={r.post_id}>
                <td>
                  <input type="checkbox" checked={selected.has(r.post_id)} onChange={() => toggleRow(r.post_id)} aria-label="Select post" />
                </td>
                <td>
                  <PostLink url={r.url} title={r.title || '(untitled)'} />
                  {(r.duplicate_count ?? 0) > 0 && (
                    <span className="chip chip-dupe" title="Duplicate link edges touch this post">
                      ⚠ {r.duplicate_count} dup
                    </span>
                  )}
                </td>
                <td style={{ maxWidth: 280 }}>
                  <UrlMono url={r.url} />
                </td>
                <td className="num">{r.inbound}</td>
                <td className="num">{r.outbound}</td>
                <td className="num mono">{r.pagerank.toFixed(4)}</td>
                <td className="num">{r.click_depth}</td>
                <td className={`num val-${scoreTone(r.link_flow_score <= 1 ? r.link_flow_score * 100 : r.link_flow_score)}`}>
                  {(r.link_flow_score <= 1 ? r.link_flow_score * 100 : r.link_flow_score).toFixed(0)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

// ---------------- Visualizer tab ----------------

function clusterColor(clusterId: string | null): string {
  if (!clusterId) return '#475569';
  let h = 0;
  for (let i = 0; i < clusterId.length; i++) h = (h * 31 + clusterId.charCodeAt(i)) >>> 0;
  return `hsl(${h % 360} 62% 52%)`;
}

const EDGE_STYLES: Record<string, CSSProperties> = {
  applied: { stroke: '#16a34a', strokeWidth: 1.6 },
  approved: { stroke: '#0d9488', strokeWidth: 1.4, strokeDasharray: '7 4' },
  suggested: { stroke: '#64748b', strokeWidth: 1.2, strokeDasharray: '2 5' },
};

/** Circular-per-cluster layout: clusters on a big ring, members on small circles. */
function computePositions(nodes: GraphReportNode[]): Map<string, { x: number; y: number }> {
  const byCluster = new Map<string, GraphReportNode[]>();
  for (const n of nodes) {
    const key = n.cluster_id ?? '__unclustered__';
    const arr = byCluster.get(key);
    if (arr) arr.push(n);
    else byCluster.set(key, [n]);
  }
  const clusters = [...byCluster.values()];
  const radii = clusters.map((members) => Math.max(70, (members.length * 76) / (2 * Math.PI)));
  const maxRadius = radii.length ? Math.max(...radii) : 70;
  const ringRadius =
    clusters.length <= 1 ? 0 : Math.max(((maxRadius * 2 + 180) * clusters.length) / (2 * Math.PI), maxRadius * 2 + 120);

  const positions = new Map<string, { x: number; y: number }>();
  clusters.forEach((members, i) => {
    const angle = (2 * Math.PI * i) / clusters.length;
    const cx = Math.cos(angle) * ringRadius;
    const cy = Math.sin(angle) * ringRadius;
    const r = radii[i] ?? 70;
    members.forEach((n, j) => {
      const a = (2 * Math.PI * j) / members.length;
      positions.set(n.id, { x: cx + Math.cos(a) * r, y: cy + Math.sin(a) * r });
    });
  });
  return positions;
}

function VisualizerTab({ siteId }: { siteId: string }) {
  const [graph, setGraph] = useState<GraphReport | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setGraph(null);
    setError(null);
    fetchGraphReport(siteId)
      .then((g) => {
        if (alive) setGraph(g);
      })
      .catch((e: unknown) => {
        if (alive) setError(e instanceof Error ? e.message : 'Failed to load graph');
      });
    return () => {
      alive = false;
    };
  }, [siteId]);

  const { rfNodes, rfEdges } = useMemo(() => {
    if (!graph) return { rfNodes: [] as Node[], rfEdges: [] as Edge[] };
    const positions = computePositions(graph.nodes);
    const maxPr = graph.nodes.reduce((m, n) => Math.max(m, n.pagerank), 0);

    const rfNodes: Node[] = graph.nodes.map((n) => {
      const size = 26 + Math.round((maxPr > 0 ? n.pagerank / maxPr : 0) * 54);
      const label = (n.title || n.url).slice(0, 22);
      return {
        id: n.id,
        position: positions.get(n.id) ?? { x: 0, y: 0 },
        data: { label },
        style: {
          width: size,
          height: size,
          borderRadius: '50%',
          background: clusterColor(n.cluster_id),
          color: '#0b1016',
          fontSize: 8,
          lineHeight: 1.1,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          textAlign: 'center' as const,
          overflow: 'hidden',
          padding: 2,
          border: n.is_orphan ? '3px solid #e11d48' : '1px solid #e2e8f2',
        },
      };
    });

    const rfEdges: Edge[] = graph.edges.map((e, i) => ({
      id: `e-${i}`,
      source: e.source,
      target: e.target,
      style: EDGE_STYLES[e.status] ?? EDGE_STYLES['suggested'],
    }));

    return { rfNodes, rfEdges };
  }, [graph]);

  if (error) return <div className="error-box">{error}</div>;
  if (!graph) return <Skeleton rows={10} />;
  if (graph.nodes.length === 0) {
    return <EmptyState title="No graph yet" hint="Run a Crawl and a Plan to build the site graph." />;
  }

  const totalNodes = graph.total_nodes ?? graph.nodes.length;
  const totalEdges = graph.total_edges ?? graph.edges.length;
  const isTruncated = graph.truncated ?? (graph.nodes.length < totalNodes || graph.edges.length < totalEdges);

  return (
    <div>
      <div className="graph-legend">
        <span>
          <span className="legend-line applied" />
          applied
        </span>
        <span>
          <span className="legend-line approved" />
          approved
        </span>
        <span>
          <span className="legend-line suggested" />
          suggested
        </span>
        <span>
          <span className="legend-ring" />
          orphan
        </span>
        <span className="faint">node color = cluster · node size = pagerank</span>
        <span className="graph-count">
          showing {graph.nodes.length.toLocaleString()} of {totalNodes.toLocaleString()} nodes ·{' '}
          {graph.edges.length.toLocaleString()} of {totalEdges.toLocaleString()} edges
        </span>
      </div>
      {isTruncated && (
        <div className="resource-notice" role="status">
          Large graph: the visualizer shows the highest-PageRank connected subset to keep this page responsive. The Links
          report still includes every post.
        </div>
      )}
      <div className="flow-wrap">
        <ReactFlow
          nodes={rfNodes}
          edges={rfEdges}
          fitView
          minZoom={0.05}
          nodesConnectable={false}
          nodesDraggable={false}
          nodesFocusable={false}
          edgesFocusable={false}
          elementsSelectable={false}
          onlyRenderVisibleElements
        >
          <Background color="#e2e8f2" gap={24} />
          <Controls showInteractive={false} />
        </ReactFlow>
      </div>
    </div>
  );
}

// ---------------- Page ----------------

export function ReportsPage() {
  const { siteId } = useParams<{ siteId: string }>();
  const [tab, setTab] = useState<ReportTab>('links');

  const renderTab = useCallback(() => {
    if (!siteId) return null;
    return tab === 'links' ? <LinksTab siteId={siteId} /> : <VisualizerTab siteId={siteId} />;
  }, [siteId, tab]);

  if (!siteId) return <EmptyState title="No site selected" hint="Pick a site from the selector above." />;

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Reports</h1>
          <div className="page-sub">Per-post link metrics and the site graph.</div>
        </div>
      </div>
      <div className="tabs">
        <button
          className={`tab${tab === 'links' ? ' active' : ''}`}
          onClick={() => setTab('links')}
          data-hint="A sortable, filterable table of per-post link metrics — inbound, outbound, PageRank, click depth and link flow."
        >
          Links
        </button>
        <button
          className={`tab${tab === 'visualizer' ? ' active' : ''}`}
          onClick={() => setTab('visualizer')}
          data-hint="A resource-bounded graph of the highest-PageRank posts, coloured by cluster and sized by PageRank."
        >
          Visualizer
        </button>
      </div>
      {renderTab()}
    </div>
  );
}
