import { useCallback, useEffect, useState } from 'react';
import { fetchAudit, isUnavailable, v2ErrorMessage } from '../api2';
import { fmtDate } from '../format';
import type { AuditRow } from '../types2';
import { Skeleton } from './Skeleton';
import { FeatureUnavailable } from './V2Notice';

const LIMITS = [50, 100, 250, 500];

function detailText(d: AuditRow['detail']): string {
  if (d === null || d === undefined) return '';
  if (typeof d === 'string') return d === '{}' ? '' : d;
  const s = JSON.stringify(d);
  return s === '{}' ? '' : s;
}

/** Settings → Activity (admin): who changed what, via UI / API token / MCP. */
export function AuditLogPanel() {
  const [limit, setLimit] = useState(100);
  const [rows, setRows] = useState<AuditRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);

  const load = useCallback(async () => {
    setRows(null);
    try {
      setRows(await fetchAudit(limit));
      setError(null);
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(v2ErrorMessage(e, 'Failed to load audit log'));
      setRows([]);
    }
  }, [limit]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <div className="card">
      <div className="table-toolbar" style={{ marginBottom: 12 }}>
        <div className="card-title" style={{ marginBottom: 0 }}>
          Audit log
        </div>
        <span className="muted" style={{ fontSize: 12.5 }}>
          Every change made through the UI, API tokens or MCP.
        </span>
        <div className="spacer" />
        {!unavailable && (
          <>
            <select className="input" value={limit} onChange={(e) => setLimit(Number(e.target.value))} aria-label="Rows">
              {LIMITS.map((n) => (
                <option key={n} value={n}>
                  {n} rows
                </option>
              ))}
            </select>
            <button className="btn btn-sm" onClick={() => void load()}>
              Refresh
            </button>
          </>
        )}
      </div>
      {unavailable ? (
        <FeatureUnavailable feature="The audit log" />
      ) : rows === null ? (
        <Skeleton rows={5} />
      ) : (
        <>
          {error && <div className="error-box" style={{ marginBottom: 10 }}>{error}</div>}
          {rows.length === 0 ? (
            <div className="muted">No audit entries yet.</div>
          ) : (
            <div className="tbl-wrap">
              <table className="tbl">
                <thead>
                  <tr>
                    <th>When</th>
                    <th>Actor</th>
                    <th>Via</th>
                    <th>Action</th>
                    <th>Target</th>
                    <th>Detail</th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => {
                    const d = detailText(r.detail);
                    return (
                      <tr key={r.id}>
                        <td className="muted" style={{ whiteSpace: 'nowrap' }}>
                          {fmtDate(r.ts)}
                        </td>
                        <td>{r.actor}</td>
                        <td>
                          <span className="chip chip-queued">{r.via}</span>
                        </td>
                        <td className="mono">{r.action}</td>
                        <td
                          className="mono-sm"
                          style={{ maxWidth: 200, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}
                          title={r.target ?? ''}
                        >
                          {r.target ?? '—'}
                        </td>
                        <td
                          className="muted mono-sm"
                          style={{ maxWidth: 320, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}
                          title={d}
                        >
                          {d || '—'}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </>
      )}
    </div>
  );
}
