import { useEffect, useState } from 'react';
import { useParams } from 'react-router-dom';
import { downloadHistoryCsv, fetchHistory } from '../api';
import { PostLink } from '../components/PostLink';
import { StatusChip } from '../components/StatusChip';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { fmtDate } from '../format';
import { useToast } from '../toast';
import type { HistoryRow } from '../types';

const PAGE_SIZE = 50;

export function HistoryPage() {
  const { siteId } = useParams<{ siteId: string }>();
  const toast = useToast();
  const [rows, setRows] = useState<HistoryRow[] | null>(null);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [downloading, setDownloading] = useState(false);

  useEffect(() => {
    if (!siteId) return;
    let alive = true;
    setRows(null);
    setError(null);
    fetchHistory(siteId, PAGE_SIZE, page * PAGE_SIZE)
      .then((r) => {
        if (alive) {
          setRows(r.rows);
          setTotal(r.total);
        }
      })
      .catch((e: unknown) => {
        if (alive) setError(e instanceof Error ? e.message : 'Failed to load history');
      });
    return () => {
      alive = false;
    };
  }, [siteId, page]);

  useEffect(() => {
    setPage(0);
  }, [siteId]);

  async function download() {
    if (!siteId || downloading) return;
    setDownloading(true);
    try {
      const blob = await downloadHistoryCsv(siteId);
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `link-history-${siteId}.csv`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'CSV download failed', 'error');
    } finally {
      setDownloading(false);
    }
  }

  if (!siteId) return <EmptyState title="No site selected" hint="Pick a site from the selector above." />;

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Link History</h1>
          <div className="page-sub">Every deployed and reverted internal link — our clean DB audit trail (no tracking params).</div>
        </div>
        <div className="spacer" />
        <button
          className="btn"
          disabled={downloading || rows === null || total === 0}
          onClick={() => void download()}
          data-hint="Download the full applied/reverted link history for this site as a CSV audit trail."
        >
          {downloading ? 'Preparing…' : 'Download CSV'}
        </button>
      </div>

      {error && <div className="error-box">{error}</div>}

      {rows === null ? (
        <Skeleton rows={8} />
      ) : rows.length === 0 ? (
        <EmptyState title="No links applied yet" hint="Approve suggestions and run Bulk Inject — applied links appear here." />
      ) : (
        <div>
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>Source → Target</th>
                  <th>Anchor</th>
                  <th>Tool</th>
                  <th>Status</th>
                  <th>Applied</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={r.edge_id}>
                    <td className="pair-cell">
                      <div className="pair">
                        <PostLink className="src" url={r.source_url} title={r.source_title || '(untitled)'} />
                        <span className="arrow">→</span>
                        <PostLink className="dst" url={r.target_url} title={r.target_title || '(untitled)'} />
                      </div>
                      <div className="pair-urls" title={`${r.source_url} → ${r.target_url}`}>
                        {r.source_url} → {r.target_url}
                      </div>
                    </td>
                    <td className="anchor-cell" title={r.anchor}>
                      “{r.anchor}”
                    </td>
                    <td className="mono muted">{r.tool}</td>
                    <td>
                      <StatusChip status={r.status} />
                    </td>
                    <td className="muted">{fmtDate(r.applied_at ?? r.decided_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <div className="pager" aria-label="Link history pagination">
            <span className="muted">
              {page * PAGE_SIZE + 1}–{Math.min((page + 1) * PAGE_SIZE, total)} of {total.toLocaleString()}
            </span>
            <div className="pager-actions">
              <button className="btn btn-sm" disabled={page === 0} onClick={() => setPage((p) => Math.max(0, p - 1))}>
                Previous
              </button>
              <button
                className="btn btn-sm"
                disabled={(page + 1) * PAGE_SIZE >= total}
                onClick={() => setPage((p) => p + 1)}
              >
                Next
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
