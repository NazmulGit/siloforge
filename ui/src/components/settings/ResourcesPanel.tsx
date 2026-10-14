// Settings → Resources & Performance: CPU/RAM caps, CPU/GPU balancing for embeddings,
// monitor thresholds, and a live CPU/RAM chart (GET /api/v2/system/metrics).

import { useEffect, useMemo, useState } from 'react';
import { fetchSystemMetrics } from '../../api2';
import { fmtMb } from '../../format';
import type { ComputeSettings, EmbedMode, MetricSample, ResourcesSettings, ServerSettings } from '../../types2';
import { AreaChart, type ChartPoint } from '../AreaChart';
import { Toggle } from '../Toggle';
import { NumInput, RangeField, RestartBanner, Row, SaveBar, SettingsGate, useDraft, useServerSettings, type ServerSettingsState } from './serverSettings';

const MODES: Array<{ id: EmbedMode; label: string; desc: string }> = [
  { id: 'auto', label: 'Auto', desc: 'Use the GPU when one is available, otherwise the CPU. The remote provider is the fallback.' },
  { id: 'gpu', label: 'GPU only', desc: 'Always embed on the GPU (DirectML). Fastest for large sites; fails over to the CPU if the GPU errors.' },
  { id: 'cpu', label: 'CPU only', desc: 'Never touch the GPU. Use this when the GPU is busy with other work, such as games or image models.' },
  { id: 'hybrid', label: 'Hybrid', desc: 'GPU and CPU work on the same job in parallel. The GPU takes about the share you set below; the CPU steals the rest.' },
  { id: 'remote', label: 'Remote first', desc: 'Send embeddings to the configured OpenAI-compatible provider first; local models are the fallback.' },
];

function LiveChart() {
  const [samples, setSamples] = useState<MetricSample[] | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let alive = true;
    const tick = () =>
      fetchSystemMetrics('1h')
        .then((r) => {
          if (alive) setSamples(r.samples);
        })
        .catch(() => {
          if (alive) setFailed(true);
        });
    void tick();
    const t = window.setInterval(() => void tick(), 10_000);
    return () => {
      alive = false;
      window.clearInterval(t);
    };
  }, []);

  const charts = useMemo(() => {
    const s = samples ?? [];
    const pts = (f: (m: MetricSample) => number): ChartPoint[] =>
      s.map((m) => ({ t: Date.parse(m.ts), v: f(m) })).filter((p) => Number.isFinite(p.t) && Number.isFinite(p.v));
    return {
      cpu: pts((m) => m.cpu_pct),
      proc: pts((m) => m.proc_cpu_pct),
      rss: pts((m) => m.proc_rss_mb),
      memTotal: s.length ? s[s.length - 1]?.mem_total_mb : undefined,
    };
  }, [samples]);

  if (failed) return <div className="muted">Live metrics are not available on this server.</div>;
  if (!samples) return <div className="faint">Loading live metrics…</div>;
  if (samples.length === 0) return <div className="muted">No samples yet. The monitor records one every few seconds.</div>;
  const pct = (v: number) => `${v.toFixed(0)}%`;
  return (
    <div className="mini-charts">
      <AreaChart title="Machine CPU" points={charts.cpu} max={100} format={pct} />
      <AreaChart title="SiloForge CPU" points={charts.proc} max={100} format={pct} color="var(--teal)" />
      <AreaChart title="SiloForge memory" points={charts.rss} format={(v) => fmtMb(v)} color="var(--purple)" />
    </div>
  );
}

function ResourcesCard({ state, s }: { state: ServerSettingsState; s: ServerSettings }) {
  const r = useDraft<ResourcesSettings>(s.resources);
  if (!s.resources) return null;
  const d = r.draft;
  const logical = d.logical_cpus ?? 0;
  const memTotal = d.mem_total_mb ?? 0;
  const capOn = d.max_memory_mb > 0;
  return (
    <div className="card">
      <div className="card-title">CPU and memory</div>
      <div className="settings-list">
        <Row
          label="CPU threads"
          desc={
            <>
              Threads in the shared work pool for scoring and embedding. 0 means automatic.{' '}
              <b>
                Using {d.cpu_threads_effective ?? '?'} of {logical || '?'} logical CPUs.
              </b>{' '}
              Applies after a restart.
            </>
          }
        >
          <RangeField label="CPU threads" value={d.cpu_threads} onChange={(v) => r.set('cpu_threads', v)} min={0} max={Math.max(1, logical || 64)} format={(v) => (v === 0 ? 'auto' : `${v} threads`)} />
        </Row>
        <Row label="Concurrent jobs" desc="How many crawl, plan or inject jobs may run at the same time. Others wait in the queue.">
          <RangeField label="Concurrent jobs" value={d.max_concurrent_jobs} onChange={(v) => r.set('max_concurrent_jobs', v)} min={1} max={16} />
        </Row>
        <Row label="Vector cache" desc="Memory kept for paragraph embeddings so plans do not reload them from disk. Larger is faster on big sites.">
          <RangeField label="Vector cache" value={d.vector_cache_mb} onChange={(v) => r.set('vector_cache_mb', v)} min={64} max={Math.min(32768, Math.max(512, memTotal || 32768))} step={64} unit="MB" format={(v) => fmtMb(v)} />
        </Row>
        <Row
          label="Memory cap"
          desc={
            <>
              Soft limit for the whole SiloForge process. Above it, the vector cache shrinks and heavy jobs wait.
              {memTotal > 0 && <> This machine has {fmtMb(memTotal)}.</>}
            </>
          }
        >
          <div className="stack-control">
            <Toggle checked={capOn} onChange={(on) => r.set('max_memory_mb', on ? Math.min(4096, memTotal || 4096) : 0)} label="Enable memory cap" />
            {capOn && (
              <RangeField label="Memory cap" value={d.max_memory_mb} onChange={(v) => r.set('max_memory_mb', v)} min={256} max={Math.max(256, memTotal || 65536)} step={256} unit="MB" format={(v) => fmtMb(v)} />
            )}
          </div>
        </Row>
        <Row label="Run jobs at low priority" desc="Background jobs yield the CPU to everything else on this machine.">
          <Toggle checked={d.low_priority_jobs} onChange={(v) => r.set('low_priority_jobs', v)} label="Run jobs at low priority" />
        </Row>
        <Row label="Wait when the machine is busy" desc="New heavy jobs wait while machine CPU stays above this level. 0 turns it off.">
          <RangeField label="Defer above CPU" value={d.defer_when_cpu_above} onChange={(v) => r.set('defer_when_cpu_above', v)} min={0} max={100} unit="%" format={(v) => (v === 0 ? 'off' : `above ${v}%`)} />
        </Row>
      </div>
      <SaveBar dirty={r.dirty} saving={state.saving} onReset={r.reset} onSave={() => void state.save({ resources: r.changes as Record<string, unknown> }, 'Resource limits saved')} />
    </div>
  );
}

function ComputeCard({ state, s }: { state: ServerSettingsState; s: ServerSettings }) {
  const c = useDraft<ComputeSettings>(s.compute);
  const [key1, setKey1] = useState('');
  const [key2, setKey2] = useState('');
  if (!s.compute) return null;
  const d = c.draft;
  const mode = String(d.embed_mode);
  const gpuOk = d.gpu_available !== false;
  const dirty = c.dirty || key1 !== '' || key2 !== '';

  async function save() {
    const patch: Record<string, unknown> = { ...c.changes };
    if (key1) patch.remote_key = key1;
    if (key2) patch.remote_key2 = key2;
    if (await state.save({ compute: patch }, 'Compute settings saved')) {
      setKey1('');
      setKey2('');
    }
  }

  return (
    <div className="card">
      <div className="card-title">CPU and GPU balancing</div>
      <div className="device-strip">
        <div>
          <div className="device-label">GPU</div>
          <div className="device-value">{d.gpu_name || (gpuOk ? 'detected' : 'none detected')}</div>
        </div>
        <div>
          <div className="device-label">Embedding on</div>
          <div className="device-value mono-sm">{d.active_device || '—'}</div>
        </div>
        <div>
          <div className="device-label">Fallback order</div>
          <div className="device-value mono-sm">{(d.embed_chain ?? []).join(' → ') || '—'}</div>
        </div>
      </div>
      <div className="mode-grid" role="radiogroup" aria-label="Embedding device">
        {MODES.map((m) => {
          const needsGpu = (m.id === 'gpu' || m.id === 'hybrid') && !gpuOk;
          return (
            <label key={m.id} className={`mode-card${mode === m.id ? ' on' : ''}${needsGpu ? ' warn' : ''}`}>
              <input type="radio" name="embed_mode" value={m.id} checked={mode === m.id} onChange={() => c.set('embed_mode', m.id)} />
              <span className="mode-title">{m.label}</span>
              <span className="mode-desc">{m.desc}</span>
              {needsGpu && <span className="mode-warn">No GPU detected: this runs on the CPU.</span>}
            </label>
          );
        })}
      </div>
      <div className="settings-list">
        {mode === 'hybrid' && (
          <Row label="GPU share" desc="Rough share of embedding batches the GPU takes in hybrid mode. The CPU picks up whatever the GPU has not claimed.">
            <RangeField
              label="GPU share"
              value={Math.round(d.hybrid_gpu_share * 100)}
              onChange={(v) => c.set('hybrid_gpu_share', Math.round(v) / 100)}
              min={10}
              max={90}
              step={5}
              unit="%"
              format={(v) => `GPU ${v}% / CPU ${100 - v}%`}
            />
          </Row>
        )}
        <Row label="Batch size" desc="Paragraphs embedded per batch. Larger batches use the GPU better but need more memory.">
          <RangeField label="Batch size" value={d.embed_batch_size} onChange={(v) => c.set('embed_batch_size', v)} min={8} max={512} step={8} />
        </Row>
        <Row label="Remote model" desc="Model name sent to the OpenAI-compatible embeddings provider.">
          <input className="input" value={d.remote_model ?? ''} onChange={(e) => c.set('remote_model', e.target.value)} aria-label="Remote model" />
        </Row>
        <Row label="Remote API key" desc={d.remote_key_set ? 'A key is saved. Type a new one to replace it.' : 'No key saved.'}>
          <input className="input" type="password" autoComplete="off" placeholder={d.remote_key_set ? '••••••••' : 'sk-…'} value={key1} onChange={(e) => setKey1(e.target.value)} aria-label="Remote API key" />
        </Row>
        <Row label="Second remote key" desc={d.remote_key2_set ? 'A key is saved. Type a new one to replace it.' : 'Optional. Used when the first key fails.'}>
          <input className="input" type="password" autoComplete="off" placeholder={d.remote_key2_set ? '••••••••' : 'optional'} value={key2} onChange={(e) => setKey2(e.target.value)} aria-label="Second remote API key" />
        </Row>
      </div>
      <SaveBar
        dirty={dirty}
        saving={state.saving}
        onReset={() => {
          c.reset();
          setKey1('');
          setKey2('');
        }}
        onSave={() => void save()}
      />
    </div>
  );
}

const MONITOR_FIELDS: Array<{ key: string; label: string; desc: string; unit?: string; min: number; max: number }> = [
  { key: 'sample_secs', label: 'Sample every', desc: 'How often CPU and memory are recorded.', unit: 's', min: 1, max: 3600 },
  { key: 'retain_days', label: 'Keep metrics for', desc: 'Older samples are deleted.', unit: 'days', min: 1, max: 365 },
  { key: 'event_log_retain_days', label: 'Keep logs for', desc: 'Retention of the unified log on the Logs page.', unit: 'days', min: 1, max: 3650 },
  { key: 'alert_cpu_pct', label: 'CPU alert', desc: 'Notify when machine CPU stays above this.', unit: '%', min: 1, max: 100 },
  { key: 'alert_mem_pct', label: 'Memory alert', desc: 'Notify when machine memory use stays above this.', unit: '%', min: 1, max: 100 },
  { key: 'alert_disk_free_mb', label: 'Low disk alert', desc: 'Notify when free disk space drops below this.', unit: 'MB', min: 0, max: 1_000_000 },
];

function MonitorCard({ state, s }: { state: ServerSettingsState; s: ServerSettings }) {
  const m = useDraft<Record<string, number>>(s.monitor);
  if (!s.monitor) return null;
  const fields = MONITOR_FIELDS.filter((f) => typeof s.monitor?.[f.key] === 'number');
  if (fields.length === 0) return null;
  return (
    <div className="card">
      <div className="card-title">Monitoring and alerts</div>
      <div className="settings-list">
        {fields.map((f) => (
          <Row key={f.key} label={f.label} desc={f.desc}>
            <NumInput label={f.label} value={m.draft[f.key] ?? 0} onChange={(v) => m.set(f.key, v)} min={f.min} max={f.max} unit={f.unit} />
          </Row>
        ))}
      </div>
      <SaveBar dirty={m.dirty} saving={state.saving} onReset={m.reset} onSave={() => void state.save({ monitor: m.changes }, 'Monitoring saved')} />
    </div>
  );
}

export function ResourcesPanel() {
  const state = useServerSettings();
  return (
    <>
      <RestartBanner state={state} />
      <div className="card">
        <div className="card-title">Live load (last hour)</div>
        <LiveChart />
      </div>
      <SettingsGate state={state}>
        {(s) => (
          <>
            <ResourcesCard state={state} s={s} />
            <ComputeCard state={state} s={s} />
            <MonitorCard state={state} s={s} />
          </>
        )}
      </SettingsGate>
    </>
  );
}
