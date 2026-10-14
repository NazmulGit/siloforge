// Auto-Linker (spec §5): scheduled Link Runs per site, stored in site settings `auto_linker`.
// Dry-run (stop at "ready" and notify) is the default; live deploys, verifies and repairs once.
import { useCallback, useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { ApiError, fetchSite, patchSiteSettings } from '../../api';
import { createLinkRun, v2ErrorMessage } from '../../api2';
import { useToast } from '../../toast';
import type { AutoLinkerConfig } from '../../types2';
import { Skeleton } from '../Skeleton';
import { Toggle } from '../Toggle';
import { OptionsForm, normalizeOptions } from './OptionsForm';

const CRON_PRESETS: Array<{ label: string; cron: string }> = [
  { label: 'Every day at 03:00', cron: '0 3 * * *' },
  { label: 'Every 6 hours', cron: '0 */6 * * *' },
  { label: 'Mondays at 03:00', cron: '0 3 * * 1' },
  { label: 'The 1st of the month', cron: '0 3 1 * *' },
];

function localTz(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';
  } catch {
    return 'UTC';
  }
}

function normalize(raw: unknown): AutoLinkerConfig {
  const o = (raw && typeof raw === 'object' ? raw : {}) as Partial<AutoLinkerConfig>;
  return {
    enabled: o.enabled === true,
    cron: typeof o.cron === 'string' && o.cron ? o.cron : '0 3 * * *',
    tz: typeof o.tz === 'string' && o.tz ? o.tz : localTz(),
    options: normalizeOptions(o.options),
    live: o.live === true,
    max_per_day: typeof o.max_per_day === 'number' ? o.max_per_day : 100,
    pause_on_failure_rate: typeof o.pause_on_failure_rate === 'number' ? o.pause_on_failure_rate : 0.2,
  };
}

function cronError(expr: string): string | null {
  const parts = expr.trim().split(/\s+/);
  if (!expr.trim()) return 'Enter a schedule.';
  if (parts.length !== 5 && parts.length !== 6) return `A cron expression has 5 fields (minute hour day month weekday). This one has ${parts.length}.`;
  if (parts.some((p) => !/^[\d*/,\-A-Za-z?LW#]+$/.test(p))) return 'The schedule contains a character cron does not understand.';
  return null;
}

export function AutoLinkerCard({ siteId }: { siteId: string }) {
  const toast = useToast();
  const navigate = useNavigate();
  const [cfg, setCfg] = useState<AutoLinkerConfig | null>(null);
  const [saved, setSaved] = useState<string>('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [unsupported, setUnsupported] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      const site = await fetchSite(siteId);
      const c = normalize(site.settings?.auto_linker);
      setCfg(c);
      setSaved(JSON.stringify(c));
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not load the Auto-Linker settings');
      setCfg(normalize(null));
    }
  }, [siteId]);

  useEffect(() => {
    void load();
  }, [load]);

  if (!cfg) return <Skeleton rows={4} />;
  const dirty = JSON.stringify(cfg) !== saved;
  const preset = CRON_PRESETS.find((p) => p.cron === cfg.cron.trim());
  const cErr = cronError(cfg.cron);

  const save = async () => {
    if (cErr) {
      setError(cErr);
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await patchSiteSettings(siteId, { auto_linker: { ...cfg, cron: cfg.cron.trim(), tz: cfg.tz.trim() } as unknown as Record<string, unknown> });
      setSaved(JSON.stringify(cfg));
      toast.push(cfg.enabled ? 'Auto-Linker saved and on' : 'Auto-Linker saved', 'success');
    } catch (e) {
      if (e instanceof ApiError && e.status === 400 && /auto_linker|unknown settings key/i.test(e.message)) {
        setUnsupported(true);
      } else setError(e instanceof Error ? e.message : 'Save failed');
    } finally {
      setSaving(false);
    }
  };

  const runNow = async () => {
    try {
      const r = await createLinkRun(siteId, cfg.options, 'Auto-Linker options (manual)');
      navigate(`/site/${siteId}/link-run?run=${encodeURIComponent(r.id)}`);
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Could not start a run'), 'error');
    }
  };

  return (
    <div className="card lr-auto">
      <div className="lr-auto-head">
        <div>
          <div className="card-title" style={{ marginBottom: 4 }}>
            Auto-Linker
          </div>
          <div className="muted" style={{ fontSize: 13 }}>
            Runs a Link Run on a schedule. In dry-run it stops at the preview and notifies you; live deploys it.
          </div>
        </div>
        <div className="spacer" />
        <Toggle checked={cfg.enabled} onChange={(v) => setCfg({ ...cfg, enabled: v })} label="Auto-Linker on" />
      </div>
      {unsupported && (
        <div className="info-note">
          <span className="info-note-icon">i</span>
          <span className="info-note-text">This server does not know the Auto-Linker yet. It needs SiloForge 2.2.</span>
        </div>
      )}
      {error && <div className="error-box">{error}</div>}

      <div className="lr-auto-grid">
        <label className="field">
          Schedule
          <select
            className="input"
            value={preset ? preset.cron : 'custom'}
            onChange={(e) => e.target.value !== 'custom' && setCfg({ ...cfg, cron: e.target.value })}
          >
            {CRON_PRESETS.map((p) => (
              <option key={p.cron} value={p.cron}>
                {p.label}
              </option>
            ))}
            <option value="custom">Custom cron…</option>
          </select>
        </label>
        <label className="field">
          Cron
          <input className="input mono" value={cfg.cron} onChange={(e) => setCfg({ ...cfg, cron: e.target.value })} aria-invalid={!!cErr} />
        </label>
        <label className="field">
          Timezone
          <input className="input" value={cfg.tz} onChange={(e) => setCfg({ ...cfg, tz: e.target.value })} placeholder="Europe/London" />
        </label>
        <label className="field">
          Daily cap (links)
          <input
            className="input"
            type="number"
            min={1}
            max={100000}
            value={cfg.max_per_day}
            onChange={(e) => setCfg({ ...cfg, max_per_day: Math.max(1, Math.round(Number(e.target.value) || 1)) })}
          />
        </label>
      </div>
      {cErr && <div className="faint lr-text-bad" style={{ fontSize: 12.5 }}>{cErr}</div>}

      <div className="lr-slider" data-hint="If more than this share of a live run fails, the Auto-Linker switches itself off and notifies you.">
        <span>Pause when failures exceed</span>
        <input
          type="range"
          min={0.05}
          max={1}
          step={0.05}
          value={cfg.pause_on_failure_rate}
          onChange={(e) => setCfg({ ...cfg, pause_on_failure_rate: Number(e.target.value) })}
          aria-label="Failure rate that pauses the Auto-Linker"
        />
        <output className="mono">{Math.round(cfg.pause_on_failure_rate * 100)}%</output>
      </div>

      <div className={`lr-live${cfg.live ? ' on' : ''}`}>
        <div className="settings-row-text">
          <div className="settings-row-label">{cfg.live ? 'Live: links are deployed automatically' : 'Dry-run: stop at the preview'}</div>
          <div className="settings-row-desc">
            {cfg.live
              ? 'Each scheduled run writes links to WordPress without review, then verifies and repairs once. Check the notifications after each run.'
              : 'Each scheduled run plans links and notifies you with “N links ready to deploy”. Nothing changes on the site until you deploy.'}
          </div>
        </div>
        <Toggle checked={cfg.live} onChange={(v) => setCfg({ ...cfg, live: v })} label="Deploy automatically (live)" />
      </div>

      <details className="lr-more">
        <summary>Run options (same as the Link Run steps)</summary>
        <div className="lr-more-body">
          <OptionsForm value={cfg.options} onChange={(o) => setCfg({ ...cfg, options: o })} siteId={siteId} />
        </div>
      </details>

      <div className="lr-step-foot">
        <button className="btn" onClick={() => void runNow()} data-hint="Starts a normal Link Run with these options now. It stops at the review step.">
          Run now
        </button>
        <div className="spacer" />
        {dirty && <span className="faint">Unsaved changes</span>}
        <button className="btn btn-primary" onClick={() => void save()} disabled={saving || !dirty}>
          {saving ? 'Saving…' : 'Save Auto-Linker'}
        </button>
      </div>
    </div>
  );
}
