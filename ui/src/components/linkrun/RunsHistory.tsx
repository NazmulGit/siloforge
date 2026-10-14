// Runs history for one site: status, counts, when, who (or Auto), open and Undo run.
import { useCallback, useEffect, useState } from 'react';
import { fetchLinkRuns, isUnavailable, linkRunAction, v2ErrorMessage } from '../../api2';
import { fmtDate } from '../../format';
import { useToast } from '../../toast';
import type { LinkRun } from '../../types2';
import { Modal } from '../Modal';
import { EmptyState, Skeleton } from '../Skeleton';
import { FeatureUnavailable } from '../V2Notice';
import { MODE_LABEL } from './OptionsForm';
import { DEPLOYED_STATUSES, RunBadge, isActive, n } from './runStatus';

export function RunsHistory({
  siteId,
  onOpen,
  refreshKey,
}: {
  siteId: string;
  onOpen: (runId: string) => void;
  refreshKey?: number;
}) {
  const toast = useToast();
  const [runs, setRuns] = useState<LinkRun[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [undoRun, setUndoRun] = useState<LinkRun | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      setRuns(await fetchLinkRuns(siteId));
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(v2ErrorMessage(e, 'Could not load runs'));
      setRuns([]);
    }
  }, [siteId]);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);

  // keep the list live while a run is busy (E2E 2.2: an undo stayed "Deploying" until Refresh)
  const anyActive = runs?.some((r) => isActive(r)) ?? false;
  useEffect(() => {
    if (!anyActive) return;
    const t = window.setInterval(() => void load(), 3000);
    return () => window.clearInterval(t);
  }, [anyActive, load]);

  const discard = async (r: LinkRun) => {
    try {
      await linkRunAction(r.id, 'cancel');
      toast.push('Run discarded. Its links are free for the next run.', 'success');
      void load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Could not discard the run'), 'error');
    }
  };

  const doUndo = async () => {
    if (!undoRun) return;
    setBusy(true);
    try {
      await linkRunAction(undoRun.id, 'undo');
      toast.push('Undo started. Every link this run applied will be removed.', 'success');
      setUndoRun(null);
      void load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Undo failed'), 'error');
    } finally {
      setBusy(false);
    }
  };

  if (unavailable) return <FeatureUnavailable feature="Link Runs (SiloForge 2.2)" />;
  if (runs === null) return <Skeleton rows={5} />;

  return (
    <div className="card lr-history">
      <div className="lr-history-head">
        <div className="card-title" style={{ margin: 0 }}>
          Runs on this site
        </div>
        <div className="spacer" />
        <button className="btn btn-sm" onClick={() => void load()}>
          Refresh
        </button>
      </div>
      {error && <div className="error-box">{error}</div>}
      {runs.length === 0 ? (
        <EmptyState title="No link runs yet" hint="Start one with the steps on the New run tab." />
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Run</th>
                <th>Status</th>
                <th className="num">Applied</th>
                <th className="num">Failed</th>
                <th className="num">Selected</th>
                <th>When</th>
                <th>By</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {runs.map((r) => {
                const s = r.stats ?? {};
                const failed = n(s.failed) + n(s.not_present);
                return (
                  <tr key={r.id}>
                    <td>
                      <button type="button" className="lr-linkish" onClick={() => onOpen(r.id)}>
                        {r.name || `Run ${r.id.slice(0, 8)}`}
                      </button>
                      <div className="faint" style={{ fontSize: 12 }}>
                        {MODE_LABEL[r.options?.mode ?? ''] ?? r.options?.mode ?? ''}
                      </div>
                    </td>
                    <td>
                      <RunBadge status={r.status} />
                    </td>
                    <td className="num">{n(s.applied)}</td>
                    <td className={`num${failed > 0 ? ' lr-text-bad' : ''}`}>{failed}</td>
                    <td className="num">{n(s.selected)}</td>
                    <td className="muted">{fmtDate(r.deployed_at ?? r.created_at)}</td>
                    <td>{r.auto ? <span className="lr-auto-badge">Auto</span> : <span className="muted">{r.created_by_name || r.created_by || 'you'}</span>}</td>
                    <td className="lr-row-actions">
                      <button className="btn btn-sm" onClick={() => onOpen(r.id)}>
                        Open
                      </button>
                      {DEPLOYED_STATUSES.has(r.status) && (
                        <button className="btn btn-sm btn-danger" onClick={() => setUndoRun(r)}>
                          Undo run
                        </button>
                      )}
                      {r.status === 'ready' && (
                        <button
                          className="btn btn-sm"
                          onClick={() => void discard(r)}
                          data-hint="Cancel this ready run without deploying. Its links become free for the next run."
                        >
                          Discard
                        </button>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {undoRun && (
        <Modal
          title="Undo this run?"
          onClose={() => setUndoRun(null)}
          footer={
            <>
              <button className="btn" onClick={() => setUndoRun(null)} disabled={busy}>
                Keep links
              </button>
              <button className="btn btn-danger" onClick={() => void doUndo()} disabled={busy}>
                {busy ? 'Undoing…' : `Remove ${n(undoRun.stats?.applied)} links`}
              </button>
            </>
          }
        >
          <p>
            Every link this run applied ({n(undoRun.stats?.applied)}) is removed from the live posts, exactly as it was
            inserted. Links added by other runs are not touched.
          </p>
        </Modal>
      )}
    </div>
  );
}
