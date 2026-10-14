import { useMemo, useState, type ReactElement } from 'react';
import { bulkDecide, decideEdge, editEdgeAnchor } from '../api';
import { useToast } from '../toast';
import type { EdgeView, ScoreBreakdown } from '../types';
import { PostLink } from './PostLink';
import { ScoreBar } from './ScoreBar';
import { StatusChip } from './StatusChip';
import { EmptyState, Skeleton } from './Skeleton';

const DIMS: Array<{ key: string; label: string }> = [
  { key: 'semantic', label: 'Semantic' },
  { key: 'orphan', label: 'Orphan' },
  { key: 'depth', label: 'Depth' },
  { key: 'tier', label: 'Tier' },
  { key: 'cluster', label: 'Cluster' },
  { key: 'velocity', label: 'Velocity' },
  { key: 'graph', label: 'Graph' },
  { key: 'money', label: 'Money' },
  { key: 'gsc', label: 'GSC' },
  { key: 'authority', label: 'Authority' },
  { key: 'placement', label: 'Placement' },
];

function BreakdownPopover({ breakdown }: { breakdown: ScoreBreakdown | Record<string, never> }) {
  const dims = breakdown as Record<string, unknown>;
  const hasData = DIMS.some((d) => typeof dims[d.key] === 'number');
  if (!hasData) {
    return (
      <div className="popover">
        <div className="popover-empty">No score breakdown available.</div>
      </div>
    );
  }
  return (
    <div className="popover">
      <div className="popover-title">Score breakdown (11 dims)</div>
      {DIMS.map((d) => {
        const raw = dims[d.key];
        const v = typeof raw === 'number' ? Math.max(0, Math.min(1, raw)) : 0;
        return (
          <div className="dim-row" key={d.key}>
            <span className="dim-label">{d.label}</span>
            <span className="dim-bar">
              <span className="dim-fill" style={{ width: `${Math.round(v * 100)}%` }} />
            </span>
            <span className="dim-val">{Math.round(v * 100)}</span>
          </div>
        );
      })}
    </div>
  );
}

export interface EdgeTableProps {
  edges: EdgeView[];
  loading: boolean;
  /** Sort + insert group header rows by source or target post. */
  groupBy?: 'source' | 'target';
  /** Put anchor + anchor_type first and emphasize them (Anchors tab). */
  anchorProminent?: boolean;
  /** Show per-row Approve/Reject on suggested edges (default true). */
  allowDecide?: boolean;
  /** Show an inline "Edit" affordance to change the anchor text (Anchors tab). */
  allowAnchorEdit?: boolean;
  emptyTitle?: string;
  emptyHint?: string;
  /** Called after any decide/bulk-decide succeeds so the parent can refetch. */
  onChanged: () => void;
}

export function EdgeTable({
  edges,
  loading,
  groupBy,
  anchorProminent = false,
  allowDecide = true,
  allowAnchorEdit = false,
  emptyTitle = 'No suggestions yet',
  emptyHint = 'Run a Plan to generate candidate links.',
  onChanged,
}: EdgeTableProps) {
  const toast = useToast();
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [openScoreId, setOpenScoreId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const sorted = useMemo(() => {
    const list = [...edges];
    if (groupBy === 'target') {
      list.sort(
        (a, b) => a.target_title.localeCompare(b.target_title) || b.composite_score - a.composite_score,
      );
    } else if (groupBy === 'source') {
      list.sort(
        (a, b) => a.source_title.localeCompare(b.source_title) || b.composite_score - a.composite_score,
      );
    } else {
      list.sort((a, b) => b.composite_score - a.composite_score);
    }
    return list;
  }, [edges, groupBy]);

  const groupCounts = useMemo(() => {
    const counts = new Map<string, number>();
    if (groupBy) {
      for (const e of sorted) {
        const key = groupBy === 'target' ? e.target_post_id : e.source_post_id;
        counts.set(key, (counts.get(key) ?? 0) + 1);
      }
    }
    return counts;
  }, [sorted, groupBy]);

  const allSelected = sorted.length > 0 && sorted.every((e) => selected.has(e.id));

  function toggleAll() {
    setSelected(allSelected ? new Set() : new Set(sorted.map((e) => e.id)));
  }

  function toggleOne(id: string) {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelected(next);
  }

  async function decide(ids: string[], decision: 'approved' | 'rejected') {
    if (ids.length === 0 || busy) return;
    setBusy(true);
    try {
      const first = ids[0];
      if (ids.length === 1 && first !== undefined) {
        await decideEdge(first, decision);
      } else {
        await bulkDecide(ids, decision);
      }
      toast.push(`${ids.length} suggestion${ids.length === 1 ? '' : 's'} ${decision}`, 'success');
      setSelected(new Set());
      onChanged();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Decision failed', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function editAnchor(e: EdgeView) {
    const next = window.prompt('Edit anchor text', e.anchor);
    if (next === null) return;
    const trimmed = next.trim();
    if (trimmed === '' || trimmed === e.anchor) return;
    setBusy(true);
    try {
      await editEdgeAnchor(e.id, trimmed);
      toast.push('Anchor updated', 'success');
      onChanged();
    } catch (err) {
      toast.push(err instanceof Error ? err.message : 'Anchor edit failed', 'error');
    } finally {
      setBusy(false);
    }
  }

  if (loading) return <Skeleton rows={8} />;
  if (sorted.length === 0) return <EmptyState title={emptyTitle} hint={emptyHint} />;

  const colCount = 8;
  const rows: ReactElement[] = [];
  let prevGroupKey: string | null = null;

  for (const e of sorted) {
    if (groupBy) {
      const key = groupBy === 'target' ? e.target_post_id : e.source_post_id;
      if (key !== prevGroupKey) {
        prevGroupKey = key;
        const title = (groupBy === 'target' ? e.target_title : e.source_title) || '(untitled)';
        const count = groupCounts.get(key) ?? 0;
        rows.push(
          <tr key={`group-${key}`} className="group-row">
            <td colSpan={colCount}>
              {groupBy === 'target' ? `Links into: ${title}` : `Links from: ${title}`}{' '}
              <span className="muted">
                · {count} suggestion{count === 1 ? '' : 's'}
              </span>
            </td>
          </tr>,
        );
      }
    }

    const linkCell = (
      <td className="pair-cell" key="link">
        <div className="pair">
          <PostLink className="src" url={e.source_url} title={e.source_title || '(untitled)'} />
          <span className="arrow">→</span>
          <PostLink className="dst" url={e.target_url} title={e.target_title || '(untitled)'} />
        </div>
        <div className="pair-urls" title={`${e.source_url} → ${e.target_url}`}>
          {e.source_url} → {e.target_url}
        </div>
      </td>
    );
    const anchorCell = (
      <td className={`anchor-cell${anchorProminent ? ' prominent' : ''}`} key="anchor" title={e.anchor}>
        “{e.anchor}”
        {allowAnchorEdit && e.status !== 'applied' && (
          <button
            className="anchor-edit"
            title="Edit anchor text"
            data-hint="Edit the clickable anchor text for this suggested link before it's injected."
            disabled={busy}
            onClick={() => void editAnchor(e)}
          >
            ✎
          </button>
        )}
      </td>
    );
    const typeCell = (
      <td key="type">
        <span className={`chip chip-type-${e.anchor_type}`}>{e.anchor_type}</span>
      </td>
    );

    rows.push(
      <tr key={e.id}>
        <td>
          <input
            type="checkbox"
            checked={selected.has(e.id)}
            onChange={() => toggleOne(e.id)}
            aria-label="Select edge"
          />
        </td>
        {anchorProminent ? [anchorCell, typeCell, linkCell] : [linkCell, anchorCell, typeCell]}
        <td className="score-cell">
          <button
            className="score-btn"
            title="Show score breakdown"
            data-hint="Show the 11-dimension relevancy breakdown (semantic, cluster, depth, authority…) behind this composite score."
            onClick={() => setOpenScoreId(openScoreId === e.id ? null : e.id)}
          >
            <ScoreBar score={e.composite_score} />
          </button>
          {openScoreId === e.id && (
            <>
              <div className="popover-backdrop" onClick={() => setOpenScoreId(null)} />
              <BreakdownPopover breakdown={e.score_breakdown} />
            </>
          )}
        </td>
        <td className="muted">{e.insert_module ?? '—'}</td>
        <td>
          <StatusChip status={e.status} />
        </td>
        <td>
          {allowDecide && e.status === 'suggested' ? (
            <div className="row-actions">
              <button
                className="btn btn-sm btn-primary"
                disabled={busy}
                onClick={() => void decide([e.id], 'approved')}
                data-hint="Approve = queue this link for injection; nothing is written until you run Bulk Inject."
              >
                Approve
              </button>
              <button
                className="btn btn-sm btn-danger"
                disabled={busy}
                onClick={() => void decide([e.id], 'rejected')}
                data-hint="Reject = discard this suggestion so it won't be injected. You can re-plan to regenerate it later."
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
  }

  return (
    <div>
      <div className="table-toolbar">
        <span className="muted">
          {sorted.length} edge{sorted.length === 1 ? '' : 's'}
        </span>
        <div className="spacer" />
        {selected.size > 0 && (
          <>
            <span className="sel-info">{selected.size} selected</span>
            <button
              className="btn btn-sm btn-primary"
              disabled={busy}
              onClick={() => void decide([...selected], 'approved')}
              data-hint="Queue every selected suggestion for injection. Still nothing is written until you run Bulk Inject."
            >
              Approve selected
            </button>
            <button
              className="btn btn-sm btn-danger"
              disabled={busy}
              onClick={() => void decide([...selected], 'rejected')}
              data-hint="Discard every selected suggestion so none of them will be injected."
            >
              Reject selected
            </button>
          </>
        )}
      </div>
      <div className="tbl-wrap">
        <table className="tbl">
          <thead>
            <tr>
              <th style={{ width: 34 }}>
                <input type="checkbox" checked={allSelected} onChange={toggleAll} aria-label="Select all" />
              </th>
              {anchorProminent ? (
                <>
                  <th>Anchor</th>
                  <th>Type</th>
                  <th>Source → Target</th>
                </>
              ) : (
                <>
                  <th>Source → Target</th>
                  <th>Anchor</th>
                  <th>Type</th>
                </>
              )}
              <th>Score</th>
              <th>Module</th>
              <th>Status</th>
              <th>Actions</th>
            </tr>
          </thead>
          <tbody>{rows}</tbody>
        </table>
      </div>
    </div>
  );
}
