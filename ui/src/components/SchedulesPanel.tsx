// Recurring schedules per site (checklist F1) — one schedule per site + job kind.
//
// The API upserts on (site_id, job_kind), so the editor is a single form that
// creates or replaces whichever kind is selected. Cron is validated client-side
// for shape only; the API's validator is the authority and its message is shown
// verbatim when it disagrees.
import { useCallback, useEffect, useState } from 'react';
import {
  ApiError,
  deleteSchedule,
  fetchSchedules,
  runScheduleNow,
  saveSchedule,
  type ScheduleInput,
} from '../api';
import { fmtDate } from '../format';
import { useJobTracker } from '../hooks';
import { useToast } from '../toast';
import {
  SCHEDULE_KINDS,
  SILO_TOPOLOGIES,
  TOPOLOGY_HINTS,
  type ScheduleKind,
  type SiloTopology,
  type SiteSchedule,
} from '../types';
import { EmptyState, Skeleton } from './Skeleton';
import { StatusChip } from './StatusChip';

const KIND_HINTS: Record<ScheduleKind, string> = {
  audit: 'The full loop: crawl → link health → plan → metrics snapshot. This is the one to schedule if you only schedule one.',
  crawl: 'Re-read the site content only. Cheap; no analysis, no suggestions.',
  plan: 'Re-score candidate links over the content already indexed.',
  link_health: 'Re-crawl every published link for status, redirect chains and rel attributes.',
};

const PRESETS: Array<{ label: string; cron: string; hint: string }> = [
  { label: 'Daily 3am', cron: '0 3 * * *', hint: 'Every day at 03:00 in the selected timezone.' },
  { label: 'Weekly Mon 3am', cron: '0 3 * * 1', hint: 'Every Monday at 03:00 in the selected timezone.' },
  { label: 'Monthly 1st 3am', cron: '0 3 1 * *', hint: 'The 1st of every month at 03:00 in the selected timezone.' },
];

const DEFAULT_CRON = '0 3 * * 1';

function localTz(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';
  } catch {
    return 'UTC';
  }
}

/**
 * Shape-only cron check mirroring the API's rules (5 or 6 fields, no @macros).
 * Deliberately permissive — the API re-validates and its error wins.
 */
function cronShapeError(expr: string): string | null {
  const trimmed = expr.trim();
  if (trimmed === '') return 'Enter a cron expression.';
  if (trimmed.startsWith('@')) return 'Macros like @daily are not supported — use a 5-field cron expression.';
  const parts = trimmed.split(/\s+/);
  if (parts.length !== 5 && parts.length !== 6) {
    return `A cron expression has 5 fields (minute hour day month weekday) — got ${parts.length}.`;
  }
  const bad = parts.find((p) => !/^[0-9a-zA-Z*,\-/?]+$/.test(p));
  if (bad) return `"${bad}" contains a character cron does not understand.`;
  return null;
}

function tzIsValid(tz: string): boolean {
  if (!tz.trim()) return false;
  try {
    new Intl.DateTimeFormat('en-US', { timeZone: tz });
    return true;
  } catch {
    return false;
  }
}

function payloadTopology(payload: Record<string, unknown>): SiloTopology | '' {
  const t = payload.topology;
  return typeof t === 'string' && (SILO_TOPOLOGIES as string[]).includes(t) ? (t as SiloTopology) : '';
}

export function SchedulesPanel({ siteId }: { siteId: string }) {
  const toast = useToast();
  const track = useJobTracker();

  const [rows, setRows] = useState<SiteSchedule[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // ---- editor state ----
  const [jobKind, setJobKind] = useState<ScheduleKind>('audit');
  const [cron, setCron] = useState(DEFAULT_CRON);
  const [tz, setTz] = useState(localTz);
  const [active, setActive] = useState(true);
  const [topology, setTopology] = useState<SiloTopology | ''>('');
  const [formError, setFormError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoadError(null);
    try {
      setRows(await fetchSchedules(siteId));
    } catch (e) {
      const msg =
        e instanceof ApiError && (e.status === 404 || e.status === 403)
          ? 'Schedules are not available for this site.'
          : e instanceof Error
            ? e.message
            : 'Failed to load schedules';
      setLoadError(msg);
      setRows([]);
    }
  }, [siteId]);

  useEffect(() => {
    setRows(null);
    void load();
  }, [load]);

  function editRow(row: SiteSchedule) {
    setJobKind(row.job_kind);
    setCron(row.cron);
    setTz(row.tz);
    setActive(row.active);
    setTopology(payloadTopology(row.payload ?? {}));
    setFormError(null);
  }

  async function submit() {
    if (busy) return;
    const shape = cronShapeError(cron);
    if (shape) {
      setFormError(shape);
      return;
    }
    if (!tzIsValid(tz)) {
      setFormError(`"${tz}" is not a timezone this browser recognises (try Europe/London or UTC).`);
      return;
    }
    setFormError(null);
    setBusy(true);
    const body: ScheduleInput = {
      job_kind: jobKind,
      cron: cron.trim(),
      tz: tz.trim(),
      active,
      payload: topology ? { topology } : {},
    };
    try {
      const saved = await saveSchedule(siteId, body);
      if (saved.warning) toast.push(saved.warning, 'info');
      else toast.push(`Schedule saved — ${jobKind} ${active ? 'active' : 'paused'}`, 'success');
      await load();
    } catch (e) {
      // The API's cron/timezone message is far more specific than anything we
      // could reconstruct here, so it is shown verbatim.
      setFormError(e instanceof Error ? e.message : 'Could not save the schedule');
    } finally {
      setBusy(false);
    }
  }

  async function toggleActive(row: SiteSchedule) {
    if (busy) return;
    setBusy(true);
    try {
      await saveSchedule(siteId, {
        job_kind: row.job_kind,
        cron: row.cron,
        tz: row.tz,
        active: !row.active,
        payload: row.payload ?? {},
      });
      toast.push(`${row.job_kind} schedule ${row.active ? 'paused' : 'resumed'}`, 'success');
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not update the schedule', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function remove(row: SiteSchedule) {
    if (busy) return;
    setBusy(true);
    try {
      await deleteSchedule(row.id);
      toast.push(`${row.job_kind} schedule deleted`, 'success');
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not delete the schedule', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function runNow() {
    if (busy) return;
    setBusy(true);
    try {
      const { job_run_id } = await runScheduleNow(siteId, topology ? { topology } : {});
      track(siteId, job_run_id, 'Audit run', () => void load());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not start the run', 'error');
    } finally {
      setBusy(false);
    }
  }

  const kindTakesTopology = jobKind === 'audit' || jobKind === 'plan';

  return (
    <>
      <div className="card">
        <div className="table-toolbar" style={{ marginBottom: 12 }}>
          <div className="card-title" style={{ marginBottom: 0 }}>
            Schedules
          </div>
          <div className="spacer" />
          <button
            className="btn btn-sm btn-primary"
            disabled={busy}
            onClick={() => void runNow()}
            data-hint="Runs a full audit right now — crawl, link health, plan and a fresh metrics snapshot — without waiting for the schedule."
          >
            Run now
          </button>
        </div>

        {loadError && (
          <div className="error-box" style={{ marginBottom: 12 }}>
            {loadError}
          </div>
        )}

        {rows === null ? (
          <Skeleton rows={3} />
        ) : rows.length === 0 ? (
          <EmptyState
            title="No schedules yet"
            hint="Internal linking is a process, not a one-off — add a recurring audit below."
          />
        ) : (
          <div className="tbl-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>Job</th>
                  <th>Cron</th>
                  <th>Timezone</th>
                  <th>State</th>
                  <th>Next run</th>
                  <th>Last run</th>
                  <th style={{ width: 190 }}>Actions</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={r.id}>
                    <td className="mono" title={KIND_HINTS[r.job_kind]}>
                      {r.job_kind}
                    </td>
                    <td className="mono">{r.cron}</td>
                    <td className="muted">{r.tz}</td>
                    <td>
                      <StatusChip status={r.active ? 'approved' : 'reverted'} />
                    </td>
                    <td className="muted">{r.active ? fmtDate(r.next_run_at) : '—'}</td>
                    <td className="muted">{fmtDate(r.last_run_at)}</td>
                    <td>
                      <div className="row-actions">
                        <button
                          className="btn btn-sm"
                          disabled={busy}
                          onClick={() => editRow(r)}
                          data-hint="Load this schedule into the editor below so you can change its cron, timezone or topology."
                        >
                          Edit
                        </button>
                        <button
                          className="btn btn-sm"
                          disabled={busy}
                          onClick={() => void toggleActive(r)}
                          data-hint="Pause or resume this schedule. Paused schedules keep their settings but never fire."
                        >
                          {r.active ? 'Pause' : 'Resume'}
                        </button>
                        <button
                          className="btn btn-sm btn-danger"
                          disabled={busy}
                          onClick={() => void remove(r)}
                          data-hint="Delete this schedule entirely. Past job runs are kept."
                        >
                          Delete
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <div className="card">
        <div className="card-title">Add / update a schedule</div>
        <div className="sched-form">
          <label className="field">
            Job kind
            <select
              className="input"
              value={jobKind}
              onChange={(e) => setJobKind(e.target.value as ScheduleKind)}
              data-hint={KIND_HINTS[jobKind]}
            >
              {SCHEDULE_KINDS.map((k) => (
                <option key={k} value={k}>
                  {k}
                </option>
              ))}
            </select>
          </label>

          <label className="field">
            Cron expression
            <input
              type="text"
              className="input mono"
              value={cron}
              onChange={(e) => setCron(e.target.value)}
              placeholder="0 3 * * 1"
              spellCheck={false}
              data-hint="Standard 5-field cron: minute hour day-of-month month day-of-week. Use a preset if you'd rather not hand-write one."
            />
          </label>

          <label className="field">
            Timezone
            <input
              type="text"
              className="input"
              value={tz}
              onChange={(e) => setTz(e.target.value)}
              placeholder="UTC"
              spellCheck={false}
              data-hint="IANA timezone the cron expression is interpreted in — e.g. UTC, Europe/London, Asia/Dhaka."
            />
          </label>

          {kindTakesTopology && (
            <label className="field">
              Topology (optional)
              <select
                className="input"
                value={topology}
                onChange={(e) => setTopology(e.target.value as SiloTopology | '')}
                data-hint="Which silo shape the scheduled plan builds. Leave on “site default” to use the site's own setting."
              >
                <option value="">site default</option>
                {SILO_TOPOLOGIES.map((t) => (
                  <option key={t} value={t}>
                    {t} — {TOPOLOGY_HINTS[t]}
                  </option>
                ))}
              </select>
            </label>
          )}

          <label className="field sched-active">
            <span>&nbsp;</span>
            <span className="dup-toggle">
              <input type="checkbox" checked={active} onChange={(e) => setActive(e.target.checked)} />
              Active
            </span>
          </label>
        </div>

        <div className="sched-presets">
          <span className="faint">Presets:</span>
          {PRESETS.map((p) => (
            <button
              key={p.cron}
              type="button"
              className={`btn btn-sm${cron.trim() === p.cron ? ' btn-primary' : ''}`}
              onClick={() => {
                setCron(p.cron);
                setFormError(null);
              }}
              data-hint={p.hint}
            >
              {p.label}
            </button>
          ))}
          <code className="mono faint">{cron.trim() || '—'}</code>
        </div>

        {formError && (
          <div className="error-box" style={{ marginTop: 12 }}>
            {formError}
          </div>
        )}

        <div className="form-actions">
          <button
            className="btn btn-primary"
            disabled={busy}
            onClick={() => void submit()}
            data-hint="Saves this schedule. One schedule exists per site and job kind — saving the same kind again replaces it."
          >
            {busy ? 'Saving…' : 'Save schedule'}
          </button>
          <span className="faint" style={{ alignSelf: 'center', fontSize: 12.5 }}>
            One schedule per job kind — saving <b>{jobKind}</b> again replaces the existing one.
          </span>
        </div>
      </div>
    </>
  );
}
