import { useEffect, useRef, useState } from 'react';
import { fetchJobRun } from '../api';
import { fetchJobLogs, isUnavailable, v2ErrorMessage } from '../api2';
import { fmtDate } from '../format';
import type { JobLogRow } from '../types2';
import { Modal } from './Modal';
import { StatusChip } from './StatusChip';

const POLL_MS = 2000;
const MAX_ROWS = 5000;

function isActive(status: string | undefined): boolean {
  return status === 'queued' || status === 'running';
}

function dataText(data: unknown): string {
  if (data === null || data === undefined || data === '') return '';
  if (typeof data === 'string') return data;
  try {
    return JSON.stringify(data);
  } catch {
    return '';
  }
}

/**
 * Log viewer for one job run. Tails GET /api/v2/jobs/:id/logs?after_id= every 2s
 * while the job is queued/running, then stops.
 */
export function JobLogDrawer({
  jobId,
  title,
  initialStatus,
  onClose,
}: {
  jobId: string;
  title: string;
  initialStatus?: string;
  onClose: () => void;
}) {
  const [rows, setRows] = useState<JobLogRow[]>([]);
  const [status, setStatus] = useState<string | undefined>(initialStatus);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [follow, setFollow] = useState(true);
  const boxRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    let alive = true;
    let timer: number | undefined;
    let lastId: number | undefined;
    let current = initialStatus;

    const tick = async () => {
      try {
        const batch = await fetchJobLogs(jobId, lastId);
        if (!alive) return;
        if (batch.length > 0) {
          lastId = batch[batch.length - 1].id;
          setRows((cur) => {
            const next = cur.concat(batch);
            return next.length > MAX_ROWS ? next.slice(next.length - MAX_ROWS) : next;
          });
        }
        setError(null);
        // Refresh the run status so polling stops once it finishes.
        try {
          const run = await fetchJobRun(jobId);
          if (!alive) return;
          current = run.status;
          setStatus(run.status);
        } catch {
          /* older server: keep the status we were given */
        }
      } catch (e) {
        if (!alive) return;
        if (isUnavailable(e)) {
          setUnavailable(true);
          return;
        }
        setError(v2ErrorMessage(e, 'Failed to load logs'));
      }
      if (alive && isActive(current)) timer = window.setTimeout(() => void tick(), POLL_MS);
    };

    void tick();
    return () => {
      alive = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [jobId, initialStatus]);

  useEffect(() => {
    if (follow && boxRef.current) boxRef.current.scrollTop = boxRef.current.scrollHeight;
  }, [rows, follow]);

  return (
    <Modal
      title={title}
      onClose={onClose}
      wide
      footer={
        <>
          <label className="muted" style={{ display: 'inline-flex', gap: 6, alignItems: 'center', marginRight: 'auto', fontSize: 13 }}>
            <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} /> Follow output
          </label>
          <button className="btn" onClick={onClose}>
            Close
          </button>
        </>
      }
    >
      <div className="table-toolbar" style={{ marginBottom: 10 }}>
        {status && <StatusChip status={status} />}
        <span className="muted mono-sm">{jobId}</span>
        <div className="spacer" />
        <span className="muted" style={{ fontSize: 12.5 }}>
          {isActive(status) ? 'Live — refreshing every 2 s' : `${rows.length} line${rows.length === 1 ? '' : 's'}`}
        </span>
      </div>
      {unavailable ? (
        <div className="muted">Job logs are not available on this server.</div>
      ) : (
        <>
          {error && (
            <div className="error-box" style={{ marginBottom: 10 }}>
              {error}
            </div>
          )}
          <div className="log-box" ref={boxRef}>
            {rows.length === 0 ? (
              <div className="faint">{isActive(status) ? 'Waiting for output…' : 'No log lines for this run.'}</div>
            ) : (
              rows.map((r) => {
                const extra = dataText(r.data);
                return (
                  <div key={r.id} className={`log-line log-${r.level}`}>
                    <span className="log-ts" title={fmtDate(r.ts)}>
                      {new Date(r.ts).toLocaleTimeString()}
                    </span>
                    <span className="log-level">{r.level}</span>
                    <span className="log-msg">
                      {r.message}
                      {extra && <span className="log-data"> {extra}</span>}
                    </span>
                  </div>
                );
              })
            )}
          </div>
        </>
      )}
    </Modal>
  );
}
