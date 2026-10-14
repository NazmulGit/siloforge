// Settings → AI Models (v2.3, admin only): "Always use AI" switch + tabs
// Chains by tier · Task routing · Budgets & cache · Usage · Providers & models.
// Contract: docs/specs/v2.3-ai-tiers.md §5–§6 (settings section `ai`, /ai/tiers, /ai/usage).

import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { fetchProvidersHealth } from '../../api';
import { fmtTokens, fmtUsd } from '../../format';
import { fetchAiTiers, fetchAiUsage, isUnavailable, v2ErrorMessage } from '../../api2';
import {
  AI_TASKS,
  AI_TIERS,
  REASONING_EFFORTS,
  type AiSettings,
  type AiTier,
  type AiTierView,
  type AiUsage,
  type AiUsageRow,
  type ReasoningEffort,
  type ServerSettings,
} from '../../types2';
import type { ModelRow } from '../../types';
import { AreaChart } from '../AreaChart';
import { ChainsPanel } from '../ChainsPanel';
import { LlmFallbackCard } from './ExtraCards';
import { ModelsPanel } from '../ModelsPanel';
import { Skeleton } from '../Skeleton';
import { Toggle } from '../Toggle';
import { NumInput, RangeField, Row, SaveBar, SettingsGate, useDraft, useServerSettings, type ServerSettingsState } from './serverSettings';

// ---------------------------------------------------------------------------------------------
// formatting

const TIER_LABEL: Record<AiTier, string> = { low: 'Low', main: 'Main', high: 'High priority' };
const TIER_HINT: Record<AiTier, string> = {
  low: 'Cheap, fast model for batched anchor picks and relevance checks.',
  main: 'Default model for sentence rewrites and repairs.',
  high: 'Stronger model for money pages and high-priority runs.',
};

function tokensOf(r: AiUsageRow): number {
  return (r.tokens_in ?? 0) + (r.tokens_out ?? 0);
}

// ---------------------------------------------------------------------------------------------
// Always use AI

function AlwaysOnCard({ state, ai }: { state: ServerSettingsState; ai: AiSettings }) {
  const [today, setToday] = useState<AiUsage | null>(null);
  useEffect(() => {
    let live = true;
    fetchAiUsage({ since: '24h', group: 'tier' })
      .then((u) => live && setToday(u))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, []);
  const tiers = today?.budget?.tiers ?? {};
  const limitTotal = AI_TIERS.reduce((a, t) => a + (ai.budget?.daily_tokens?.[t] ?? 0), 0);
  return (
    <div className={`card ai-always${ai.always_on ? ' on' : ''}`}>
      <div className="settings-list">
        <Row
          label={<b>Always use AI model for perfect linking</b>}
          desc={
            <>
              Every plan and Link Run asks the AI to refine anchors and check relevance for each borderline candidate
              (low tier, batched {ai.batch_size} per request and cached for {ai.cache_ttl_days} days), lets the main tier
              write contextual sentences, and sends money-page targets to the high tier. Daily budgets are respected:
              when the chain is down or a budget is used up, linking falls back to the heuristics automatically and the
              run says so.
            </>
          }
        >
          <Toggle
            checked={!!ai.always_on}
            disabled={state.saving}
            onChange={(v) => void state.save({ ai: { always_on: v } }, v ? 'Always use AI is on' : 'Always use AI is off')}
            label="Always use AI model for perfect linking"
          />
        </Row>
      </div>
      <div className="faint" style={{ fontSize: 12.5, marginTop: 6 }}>
        Today: <b>{fmtTokens(today?.totals.tokens_in ?? 0)}</b> in / <b>{fmtTokens(today?.totals.tokens_out ?? 0)}</b> out
        tokens, {fmtUsd(today?.totals.cost_usd)} · {AI_TIERS.map((t) => `${t} ${fmtTokens(tiers[t]?.used ?? 0)}`).join(' · ')}
        {limitTotal > 0 ? ` of ${fmtTokens(limitTotal)} daily budget` : ' (no daily budget)'}. The Link Run wizard shows
        the estimated tokens before each run.
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Chains by tier

function EffectiveChain({ view }: { view: AiTierView | undefined }) {
  if (!view) return <div className="faint">No information about the effective chain.</div>;
  const from =
    view.resolved_from === 'tier'
      ? `its own ${view.source} chain`
      : view.resolved_from === 'main'
        ? 'the main chain (no active chain for this tier)'
        : view.resolved_from === 'env'
          ? 'the environment fallback (no active chain)'
          : 'nothing — no chat model is configured';
  return (
    <div className="info-note" style={{ marginBottom: 14 }}>
      <span className="info-note-icon" aria-hidden="true">
        ⇢
      </span>
      <span className="info-note-text">
        <b>{TIER_LABEL[view.tier] ?? view.tier}</b> tier runs on {from}
        {view.chain_name ? (
          <>
            : <b>{view.chain_name}</b>
          </>
        ) : null}
        {view.rungs.length > 0 && (
          <>
            {' '}
            —{' '}
            {view.rungs.map((r, i) => (
              <span key={`${r.model_id}-${i}`} className={r.usable === false ? 'faint' : undefined} title={r.skip_reason ?? undefined}>
                {i > 0 ? ' → ' : ''}
                <span className="mono">{r.model_id}</span>
              </span>
            ))}
          </>
        )}
        {view.reasoning_effort && view.reasoning_effort !== 'none' ? ` · reasoning ${view.reasoning_effort}` : ''}
      </span>
    </div>
  );
}

function ChainsByTier() {
  const [tier, setTier] = useState<AiTier>('low');
  const [models, setModels] = useState<ModelRow[]>([]);
  const [names, setNames] = useState<Record<string, string>>({});
  const [indexDim, setIndexDim] = useState(384);
  const [views, setViews] = useState<AiTierView[] | null>(null);
  const [viewErr, setViewErr] = useState<string | null>(null);

  const loadViews = useCallback(async () => {
    try {
      setViews(await fetchAiTiers());
      setViewErr(null);
    } catch (e) {
      setViews([]);
      setViewErr(isUnavailable(e) ? 'This server does not report effective tier chains (needs SiloForge 2.3).' : v2ErrorMessage(e));
    }
  }, []);

  useEffect(() => {
    void loadViews();
    fetchProvidersHealth()
      .then((h) => {
        setModels(h.providers.flatMap((p) => p.models));
        const n: Record<string, string> = {};
        for (const p of h.providers) n[p.id] = p.name;
        setNames(n);
        if (typeof h.index_dim === 'number') setIndexDim(h.index_dim);
      })
      .catch(() => undefined);
  }, [loadViews]);

  return (
    <>
      <div className="card">
        <div className="card-title">Chains by tier</div>
        <p className="muted" style={{ marginTop: 0, fontSize: 13 }}>
          Each AI task runs on one tier. A tier uses its own active chain (site chain first, then global); with none
          it uses the main chain, then the environment fallback. Drag rungs (or use ↑ ↓) to change the fallback order.
        </p>
        <div className="segmented" role="radiogroup" aria-label="Tier" style={{ marginBottom: 12 }}>
          {AI_TIERS.map((t) => (
            <button key={t} type="button" role="radio" aria-checked={tier === t} className={tier === t ? 'on' : ''} onClick={() => setTier(t)}>
              {TIER_LABEL[t]}
            </button>
          ))}
        </div>
        <div className="faint" style={{ fontSize: 12.5, marginBottom: 10 }}>
          {TIER_HINT[tier]}
        </div>
        {viewErr ? <div className="faint">{viewErr}</div> : views === null ? <Skeleton rows={1} /> : <EffectiveChain view={views.find((v) => v.tier === tier)} />}
      </div>
      <ChainsPanel key={tier} tier={tier} models={models} providerNames={names} indexDim={indexDim} onChanged={() => void loadViews()} />
    </>
  );
}

// ---------------------------------------------------------------------------------------------
// Task routing

function TaskRouting({ state, ai }: { state: ServerSettingsState; ai: AiSettings }) {
  const v = useDraft<AiSettings>(ai);
  const d = v.draft;
  const tiers = d.task_tiers ?? {};
  const effort = d.reasoning_effort ?? { low: 'low', main: 'low', high: 'medium' };
  const skip = d.confidence_skip ?? { anchor: 0.85, relevance: 0.8 };
  return (
    <div className="card">
      <div className="card-title">Task routing</div>
      <p className="muted" style={{ marginTop: 0, fontSize: 13 }}>
        Which tier each kind of AI work uses. Money-page and high-priority overrides win over the task&apos;s own tier.
      </p>
      <table className="tbl">
        <thead>
          <tr>
            <th>Task</th>
            <th>What it does</th>
            <th style={{ width: 150 }}>Tier</th>
          </tr>
        </thead>
        <tbody>
          {AI_TASKS.map((t) => (
            <tr key={t.key}>
              <td>
                <b>{t.label}</b>
                <div className="mono-sm faint">{t.key}</div>
              </td>
              <td className="muted">{t.what}</td>
              <td>
                <select
                  className="input"
                  value={tiers[t.key] ?? t.def}
                  aria-label={`Tier for ${t.label}`}
                  onChange={(e) => v.set('task_tiers', { ...tiers, [t.key]: e.target.value as AiTier })}
                >
                  {AI_TIERS.map((x) => (
                    <option key={x} value={x}>
                      {TIER_LABEL[x]}
                    </option>
                  ))}
                </select>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="settings-list" style={{ marginTop: 14 }}>
        {AI_TIERS.map((t) => (
          <Row
            key={t}
            label={`Reasoning effort · ${TIER_LABEL[t]}`}
            desc="Sent to reasoning models (Codex / GPT-5+ / o-series) only. Lower = fewer hidden reasoning tokens."
          >
            <select
              className="input"
              value={effort[t] ?? 'low'}
              aria-label={`Reasoning effort for the ${t} tier`}
              onChange={(e) => v.set('reasoning_effort', { ...effort, [t]: e.target.value as ReasoningEffort })}
            >
              {REASONING_EFFORTS.map((x) => (
                <option key={x} value={x}>
                  {x}
                </option>
              ))}
            </select>
          </Row>
        ))}
        <Row label="Batch size" desc="Edges sent per low-tier request (anchor picks, relevance checks).">
          <RangeField label="Batch size" min={1} max={50} value={d.batch_size ?? 20} onChange={(n) => v.set('batch_size', n)} />
        </Row>
        <Row label="Skip AI when the anchor is this confident" desc="Heuristic anchor confidence 0–1 at or above this needs no AI call.">
          <RangeField
            label="Anchor confidence threshold"
            min={0}
            max={1}
            step={0.01}
            value={skip.anchor}
            onChange={(n) => v.set('confidence_skip', { ...skip, anchor: n })}
          />
        </Row>
        <Row label="Skip AI when relevance is this high" desc="Embedding relevance 0–1 at or above this needs no relevance check.">
          <RangeField
            label="Relevance threshold"
            min={0}
            max={1}
            step={0.01}
            value={skip.relevance}
            onChange={(n) => v.set('confidence_skip', { ...skip, relevance: n })}
          />
        </Row>
      </div>
      <SaveBar
        dirty={v.dirty}
        saving={state.saving}
        onReset={v.reset}
        onSave={() => void state.save({ ai: v.changes as Record<string, unknown> }, 'AI routing saved')}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Budgets & cache

function BudgetBar({ label, used, limit }: { label: string; used: number; limit: number }) {
  const pct = limit > 0 ? Math.min(100, (used / limit) * 100) : 0;
  const over = limit > 0 && used >= limit;
  return (
    <div className="mini-progress" style={{ marginBottom: 10 }}>
      <div className="mini-progress-label">
        <b>{label}</b> · {fmtTokens(used)} {limit > 0 ? `of ${fmtTokens(limit)} (${pct.toFixed(0)}%)` : 'used · unlimited'}
        {over ? ' · exhausted, heuristics only until 00:00 UTC' : ''}
      </div>
      <div className="mini-progress-bar">
        <div className="mini-progress-fill" style={{ width: `${limit > 0 ? pct : 0}%`, background: over ? 'var(--red)' : pct > 80 ? 'var(--amber)' : undefined }} />
      </div>
    </div>
  );
}

function BudgetsCache({ state, ai }: { state: ServerSettingsState; ai: AiSettings }) {
  const v = useDraft<AiSettings>(ai);
  const d = v.draft;
  const budget = d.budget ?? { daily_tokens: { low: 0, main: 0, high: 0 }, per_site_daily_tokens: 0 };
  const [usage, setUsage] = useState<AiUsage | null>(null);
  useEffect(() => {
    fetchAiUsage({ since: '24h', group: 'tier' })
      .then(setUsage)
      .catch(() => setUsage(null));
  }, [ai]);
  const tiers = usage?.budget?.tiers ?? {};
  return (
    <>
      <div className="card">
        <div className="card-title">Today&apos;s budget (UTC)</div>
        {AI_TIERS.map((t) => (
          <BudgetBar key={t} label={TIER_LABEL[t]} used={tiers[t]?.used ?? 0} limit={tiers[t]?.limit ?? ai.budget?.daily_tokens?.[t] ?? 0} />
        ))}
        {(usage?.budget?.sites ?? []).length > 0 && (
          <>
            <div className="card-title" style={{ marginTop: 14 }}>
              Sites today
            </div>
            {(usage?.budget?.sites ?? []).map((s) => (
              <BudgetBar key={s.site_id} label={s.name || s.site_id} used={s.used} limit={s.limit} />
            ))}
          </>
        )}
      </div>
      <div className="card">
        <div className="card-title">Budgets & cache</div>
        <div className="settings-list">
          {AI_TIERS.map((t) => (
            <Row key={t} label={`Daily tokens · ${TIER_LABEL[t]}`} desc="Input + output tokens per UTC day. 0 = unlimited.">
              <NumInput
                label={`Daily tokens for the ${t} tier`}
                min={0}
                max={10_000_000_000}
                unit="tokens"
                value={budget.daily_tokens?.[t] ?? 0}
                onChange={(n) => v.set('budget', { ...budget, daily_tokens: { ...budget.daily_tokens, [t]: n } })}
              />
            </Row>
          ))}
          <Row label="Daily tokens per site" desc="Cap for any single site, all tiers together. 0 = unlimited.">
            <NumInput
              label="Daily tokens per site"
              min={0}
              max={10_000_000_000}
              unit="tokens"
              value={budget.per_site_daily_tokens ?? 0}
              onChange={(n) => v.set('budget', { ...budget, per_site_daily_tokens: n })}
            />
          </Row>
          <Row label="Cache answers for" desc="Same task + model family + inputs reuse the stored answer for free. 0 = cache off.">
            <RangeField label="Cache TTL" min={0} max={365} unit="days" value={d.cache_ttl_days ?? 30} onChange={(n) => v.set('cache_ttl_days', n)} />
          </Row>
        </div>
        <div className="faint" style={{ fontSize: 12.5 }}>
          When a budget is used up SiloForge keeps linking with heuristics only, notes it in the run stats and sends one
          “AI budget exhausted” notification per day.
        </div>
        <SaveBar
          dirty={v.dirty}
          saving={state.saving}
          onReset={v.reset}
          onSave={() => void state.save({ ai: v.changes as Record<string, unknown> }, 'AI budgets saved')}
        />
      </div>
    </>
  );
}

// ---------------------------------------------------------------------------------------------
// Usage

const WINDOWS = [
  { id: '24h', label: '24 h' },
  { id: '7d', label: '7 days' },
  { id: '30d', label: '30 days' },
];

function Bars({ title, rows, max = 8 }: { title: string; rows: AiUsageRow[]; max?: number }) {
  const top = [...rows].sort((a, b) => tokensOf(b) - tokensOf(a)).slice(0, max);
  const peak = Math.max(1, ...top.map(tokensOf));
  return (
    <div className="card">
      <div className="card-title">{title}</div>
      {top.length === 0 ? (
        <div className="faint">No AI calls in this window.</div>
      ) : (
        top.map((r) => (
          <div key={r.key} className="mini-progress" style={{ marginBottom: 8 }}>
            <div className="mini-progress-label">
              <b>{r.label || r.key}</b> · {fmtTokens(tokensOf(r))} tokens · {r.calls} calls
              {r.cache_hits ? ` · ${r.cache_hits} cache hits` : ''}
              {r.errors ? ` · ${r.errors} errors` : ''} · {fmtUsd(r.cost_usd)}
            </div>
            <div className="mini-progress-bar">
              <div className="mini-progress-fill" style={{ width: `${(tokensOf(r) / peak) * 100}%` }} />
            </div>
          </div>
        ))
      )}
    </div>
  );
}

function UsageTab() {
  const [win, setWin] = useState('7d');
  const [data, setData] = useState<Record<string, AiUsage> | null>(null);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    setData(null);
    Promise.all((['day', 'tier', 'model', 'site', 'task'] as const).map((g) => fetchAiUsage({ since: win, group: g }).then((u) => [g, u] as const)))
      .then((pairs) => {
        if (!live) return;
        setData(Object.fromEntries(pairs));
        setErr(null);
      })
      .catch((e) => live && setErr(isUnavailable(e) ? 'AI usage needs SiloForge 2.3 on the server.' : v2ErrorMessage(e)));
    return () => {
      live = false;
    };
  }, [win]);
  const day = data?.day;
  const points = useMemo(
    () =>
      (day?.rows ?? [])
        .map((r) => ({ t: Date.parse(r.key.length === 10 ? `${r.key}T00:00:00Z` : r.key), v: tokensOf(r) }))
        .filter((p) => Number.isFinite(p.t)),
    [day],
  );
  const costPoints = useMemo(
    () =>
      (day?.rows ?? [])
        .map((r) => ({ t: Date.parse(r.key.length === 10 ? `${r.key}T00:00:00Z` : r.key), v: r.cost_usd ?? 0 }))
        .filter((p) => Number.isFinite(p.t)),
    [day],
  );
  const t = day?.totals;
  return (
    <>
      <div className="card">
        <div className="table-toolbar" style={{ marginBottom: 12 }}>
          <div className="card-title" style={{ marginBottom: 0 }}>
            AI usage
          </div>
          <div className="spacer" />
          <div className="tabs" style={{ marginBottom: 0, borderBottom: 'none' }} role="tablist" aria-label="Time window">
            {WINDOWS.map((w) => (
              <button key={w.id} type="button" role="tab" aria-selected={win === w.id} className={`tab${win === w.id ? ' active' : ''}`} onClick={() => setWin(w.id)}>
                {w.label}
              </button>
            ))}
          </div>
        </div>
        {err ? (
          <div className="faint">{err}</div>
        ) : !data ? (
          <Skeleton rows={3} />
        ) : (
          <div className="ai-usage-totals">
            <div>
              <div className="faint">Calls</div>
              <b>{(t?.calls ?? 0).toLocaleString()}</b>
            </div>
            <div>
              <div className="faint">Tokens in / out</div>
              <b>
                {fmtTokens(t?.tokens_in)} / {fmtTokens(t?.tokens_out)}
              </b>
            </div>
            <div>
              <div className="faint">Cost</div>
              <b>{fmtUsd(t?.cost_usd)}</b>
            </div>
            <div>
              <div className="faint">Cache hit rate</div>
              <b>{typeof t?.cache_hit_rate === 'number' ? `${(t.cache_hit_rate * 100).toFixed(0)}%` : '—'}</b>
              <span className="faint"> ({(t?.cache_hits ?? 0).toLocaleString()} hits)</span>
            </div>
            <div>
              <div className="faint">Errors</div>
              <b>{(t?.errors ?? 0).toLocaleString()}</b>
            </div>
            {(t?.estimated ?? 0) > 0 && (
              <div>
                <div className="faint">Estimated rows</div>
                <b title="The provider sent no usage; tokens were estimated as characters / 4.">{t?.estimated}</b>
              </div>
            )}
          </div>
        )}
      </div>
      {data && (
        <>
          <div className="chart-grid">
            <AreaChart title="Tokens per day" points={points} format={fmtTokens} hint="Input + output tokens of real AI calls (cache hits are free)." />
            <AreaChart title="Cost per day" points={costPoints} format={fmtUsd} color="var(--teal)" hint="From models.price_in / price_out per 1M tokens; local proxies are $0 unless priced." />
          </div>
          <div className="chart-grid">
            <Bars title="By tier" rows={data.tier?.rows ?? []} />
            <Bars title="By model" rows={data.model?.rows ?? []} />
            <Bars title="By task" rows={data.task?.rows ?? []} />
            <Bars title="Top sites" rows={data.site?.rows ?? []} max={10} />
          </div>
        </>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------------------------
// panel

type Tab = 'chains' | 'routing' | 'budgets' | 'usage' | 'providers';
const TABS: { id: Tab; label: string; hint: string }[] = [
  { id: 'chains', label: 'Chains by tier', hint: 'Low / main / high fallback chains and the chain each tier really uses.' },
  { id: 'routing', label: 'Task routing', hint: 'Which tier each AI task uses, reasoning effort, batching and skip thresholds.' },
  { id: 'budgets', label: 'Budgets & cache', hint: 'Daily token budgets per tier and per site, and the answer cache.' },
  { id: 'usage', label: 'Usage', hint: 'Tokens, cost and cache hits by day, tier, model, task and site.' },
  { id: 'providers', label: 'Providers & models', hint: 'LLM endpoints, the models under them, all chains and the env fallback.' },
];

function NeedsAi({ s, children }: { s: ServerSettings; children: (ai: AiSettings) => ReactNode }) {
  if (!s.ai) {
    return (
      <div className="card">
        <div className="faint">AI tiers, budgets and usage need SiloForge 2.3 on the server.</div>
      </div>
    );
  }
  return <>{children(s.ai)}</>;
}

export function AiModelsPanel() {
  const state = useServerSettings();
  const [tab, setTab] = useState<Tab>('chains');
  return (
    <>
      {!state.unavailable && !state.forbidden && (
        <SettingsGate state={state}>{(s) => (s.ai ? <AlwaysOnCard state={state} ai={s.ai} /> : null)}</SettingsGate>
      )}
      <div className="tabs" role="tablist" aria-label="AI Models views">
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            id={`ai-tab-${t.id}`}
            aria-selected={tab === t.id}
            aria-controls={`ai-panel-${t.id}`}
            className={`tab${tab === t.id ? ' active' : ''}`}
            data-hint={t.hint}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div id={`ai-panel-${tab}`} role="tabpanel" aria-labelledby={`ai-tab-${tab}`} tabIndex={-1} className="ai-panel">
        {tab === 'chains' && <ChainsByTier />}
        {tab === 'routing' && (
          <SettingsGate state={state}>{(s) => <NeedsAi s={s}>{(ai) => <TaskRouting state={state} ai={ai} />}</NeedsAi>}</SettingsGate>
        )}
        {tab === 'budgets' && (
          <SettingsGate state={state}>{(s) => <NeedsAi s={s}>{(ai) => <BudgetsCache state={state} ai={ai} />}</NeedsAi>}</SettingsGate>
        )}
        {tab === 'usage' && <UsageTab />}
        {tab === 'providers' && (
          <>
            <ModelsPanel />
            <LlmFallbackCard />
          </>
        )}
      </div>
    </>
  );
}
