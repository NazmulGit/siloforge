// Shared plumbing for the server-wide settings sections (GET/PUT /api/v2/settings):
// a loader hook, a per-section draft, the "restart required" banner and small field widgets.

import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { ApiError } from '../../api';
import { fetchServerSettings, isUnavailable, pingHealth, putServerSettings, restartServer, v2ErrorMessage } from '../../api2';
import { useToast } from '../../toast';
import type { ServerSettings, SettingsPatch } from '../../types2';
import { FeatureUnavailable } from '../V2Notice';
import { Skeleton } from '../Skeleton';

export interface ServerSettingsState {
  settings: ServerSettings | null;
  unavailable: boolean;
  forbidden: boolean;
  error: string | null;
  saving: boolean;
  /** PUT a partial patch; resolves true on success (and refreshes `settings`). */
  save: (patch: SettingsPatch, okMsg?: string) => Promise<boolean>;
  reload: () => Promise<void>;
}

export function useServerSettings(): ServerSettingsState {
  const toast = useToast();
  const [settings, setSettings] = useState<ServerSettings | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [forbidden, setForbidden] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const reload = useCallback(async () => {
    try {
      setSettings(await fetchServerSettings());
      setError(null);
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else if (e instanceof ApiError && e.status === 403) setForbidden(true);
      else setError(v2ErrorMessage(e, 'Failed to load server settings'));
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  const save = useCallback(
    async (patch: SettingsPatch, okMsg = 'Settings saved') => {
      setSaving(true);
      try {
        const next = await putServerSettings(patch);
        setSettings(next);
        const pending = next.restart_required ?? [];
        toast.push(pending.length ? `${okMsg}. Some changes apply after a restart.` : okMsg, 'success');
        return true;
      } catch (e) {
        toast.push(v2ErrorMessage(e, 'Could not save settings'), 'error');
        return false;
      } finally {
        setSaving(false);
      }
    },
    [toast],
  );

  return { settings, unavailable, forbidden, error, saving, save, reload };
}

/** Renders loading / unavailable / forbidden / error states, else children(settings). */
export function SettingsGate({ state, children }: { state: ServerSettingsState; children: (s: ServerSettings) => ReactNode }) {
  if (state.unavailable)
    return (
      <div className="card">
        <FeatureUnavailable feature="Server settings" />
      </div>
    );
  if (state.forbidden)
    return (
      <div className="card">
        <div className="empty">
          <div className="empty-title">Admins only</div>
          <div className="empty-hint">Server-wide settings can be changed by an admin account.</div>
        </div>
      </div>
    );
  if (state.error) return <div className="error-box">{state.error}</div>;
  if (!state.settings) return <Skeleton rows={6} />;
  return <>{children(state.settings)}</>;
}

/** Local editable copy of one settings section, with dirty tracking. */
export function useDraft<T extends object>(value: T | undefined): {
  draft: T;
  set: <K extends keyof T>(k: K, v: T[K]) => void;
  dirty: boolean;
  changes: Partial<T>;
  reset: () => void;
} {
  const base = useMemo(() => (value ?? {}) as T, [value]);
  const [draft, setDraft] = useState<T>(base);
  useEffect(() => setDraft(base), [base]);
  const changes = useMemo(() => {
    const out: Partial<T> = {};
    for (const k of Object.keys(draft) as Array<keyof T>) {
      if (JSON.stringify(draft[k]) !== JSON.stringify(base[k])) out[k] = draft[k];
    }
    return out;
  }, [draft, base]);
  return {
    draft,
    set: (k, v) => setDraft((d) => ({ ...d, [k]: v })),
    dirty: Object.keys(changes).length > 0,
    changes,
    reset: () => setDraft(base),
  };
}

type RestartPhase = 'idle' | 'requested' | 'down' | 'back' | 'timeout';

/** Banner shown while settings that need a restart are pending; Restart → poll /health. */
export function RestartBanner({ state }: { state: ServerSettingsState }) {
  const toast = useToast();
  const [phase, setPhase] = useState<RestartPhase>('idle');
  const pending = state.settings?.restart_required ?? [];
  const supported = state.settings?.restart_supported !== false;

  async function restart() {
    if (!window.confirm('Restart SiloForge now? Running jobs are interrupted and resume or fail; the UI reconnects in a few seconds.')) return;
    setPhase('requested');
    try {
      await restartServer();
    } catch (e) {
      // The connection may drop as the process exits; only a clean error response is a failure.
      if (e instanceof ApiError && e.status !== 0) {
        toast.push(v2ErrorMessage(e, 'Restart failed'), 'error');
        setPhase('idle');
        return;
      }
    }
    const started = Date.now();
    let sawDown = false;
    while (Date.now() - started < 120_000) {
      await new Promise((r) => window.setTimeout(r, 1500));
      const up = await pingHealth();
      if (!up) {
        sawDown = true;
        setPhase('down');
      } else if (sawDown || Date.now() - started > 8000) {
        setPhase('back');
        toast.push('SiloForge restarted', 'success');
        await state.reload();
        window.setTimeout(() => setPhase('idle'), 4000);
        return;
      }
    }
    setPhase('timeout');
  }

  if (phase !== 'idle') {
    return (
      <div className={`restart-banner${phase === 'back' ? ' ok' : ''}`} role="status">
        <span className="restart-text">
          {phase === 'requested' && 'Restart requested. Waiting for the server to go down…'}
          {phase === 'down' && 'Restarting. Waiting for the server to come back…'}
          {phase === 'back' && 'Back online with the new settings.'}
          {phase === 'timeout' && 'The server has not come back after 2 minutes. Check the service on the host.'}
        </span>
        {phase === 'timeout' && (
          <button className="btn btn-sm" onClick={() => setPhase('idle')}>
            Dismiss
          </button>
        )}
      </div>
    );
  }
  if (pending.length === 0) return null;
  return (
    <div className="restart-banner" role="status">
      <span className="restart-text">
        <b>Restart required</b> to apply: <span className="mono-sm">{pending.join(', ')}</span>
      </span>
      {supported ? (
        <button className="btn btn-sm btn-primary" onClick={() => void restart()}>
          Restart now
        </button>
      ) : (
        <span className="faint">Restart the SiloForge service on the host.</span>
      )}
    </div>
  );
}

/** Save / Reset bar shown under a section form. */
export function SaveBar({ dirty, saving, onSave, onReset }: { dirty: boolean; saving: boolean; onSave: () => void; onReset: () => void }) {
  return (
    <div className="save-bar">
      <span className="faint">{dirty ? 'Unsaved changes' : 'All changes saved'}</span>
      <div className="spacer" />
      <button className="btn btn-sm" disabled={!dirty || saving} onClick={onReset}>
        Reset
      </button>
      <button className="btn btn-sm btn-primary" disabled={!dirty || saving} onClick={onSave}>
        {saving ? 'Saving…' : 'Save changes'}
      </button>
    </div>
  );
}

/** One labelled setting row: text on the left, control on the right. */
export function Row({ label, desc, children, wide }: { label: ReactNode; desc?: ReactNode; children: ReactNode; wide?: boolean }) {
  return (
    <div className={`settings-row${wide ? ' settings-row-wide' : ''}`}>
      <div className="settings-row-text">
        <div className="settings-row-label">{label}</div>
        {desc && <div className="settings-row-desc">{desc}</div>}
      </div>
      <div className="settings-row-control">{children}</div>
    </div>
  );
}

/** Slider + number box kept in sync and clamped to [min, max]. */
export function RangeField({
  value,
  onChange,
  min,
  max,
  step = 1,
  unit,
  label,
  format,
}: {
  value: number;
  onChange: (v: number) => void;
  min: number;
  max: number;
  step?: number;
  unit?: string;
  label: string;
  format?: (v: number) => string;
}) {
  const clamp = (v: number) => Math.min(max, Math.max(min, v));
  const safe = Number.isFinite(value) ? value : min;
  return (
    <div className="range-field">
      <input
        type="range"
        min={min}
        max={max}
        step={step}
        value={clamp(safe)}
        aria-label={label}
        onChange={(e) => onChange(Number(e.target.value))}
      />
      <span className="range-num">
        <input
          type="number"
          className="input"
          min={min}
          max={max}
          step={step}
          value={safe}
          aria-label={`${label} value`}
          onChange={(e) => {
            const n = Number(e.target.value);
            if (Number.isFinite(n)) onChange(n);
          }}
          onBlur={() => onChange(clamp(safe))}
        />
        {unit && <span className="range-unit">{unit}</span>}
      </span>
      {format && <span className="range-fmt faint">{format(safe)}</span>}
    </div>
  );
}

/** Plain number input with optional unit. */
export function NumInput({ value, onChange, min, max, unit, label }: { value: number; onChange: (v: number) => void; min?: number; max?: number; unit?: string; label: string }) {
  return (
    <span className="range-num">
      <input
        type="number"
        className="input"
        min={min}
        max={max}
        value={Number.isFinite(value) ? value : ''}
        aria-label={label}
        onChange={(e) => {
          const n = Number(e.target.value);
          if (e.target.value !== '' && Number.isFinite(n)) onChange(n);
        }}
      />
      {unit && <span className="range-unit">{unit}</span>}
    </span>
  );
}
