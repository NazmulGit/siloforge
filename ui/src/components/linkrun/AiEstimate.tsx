// v2.3 (docs/specs/v2.3-ai-tiers.md §4, §6): token/cost estimate before a Link Run starts,
// and the AI summary of a finished run (run.ai).

import { useEffect, useState } from 'react';
import { estimateLinkRun, isUnavailable } from '../../api2';
import { fmtTokens, fmtUsd } from '../../format';
import type { LinkRunAiSummary, LinkRunEstimate, LinkRunOptions } from '../../types2';

const DEBOUNCE_MS = 500;

/** Estimated AI tokens/cost for these options. Hidden on servers without the estimate route. */
export function AiEstimateCard({ siteId, options }: { siteId: string | null; options: LinkRunOptions }) {
  const [est, setEst] = useState<LinkRunEstimate | null>(null);
  const [state, setState] = useState<'idle' | 'loading' | 'error' | 'unavailable'>('idle');
  const key = JSON.stringify(options);

  useEffect(() => {
    if (!siteId) return;
    let live = true;
    setState('loading');
    const t = window.setTimeout(() => {
      estimateLinkRun(siteId, options)
        .then((e) => {
          if (!live) return;
          setEst(e);
          setState('idle');
        })
        .catch((e) => {
          if (!live) return;
          setState(isUnavailable(e) ? 'unavailable' : 'error');
        });
    }, DEBOUNCE_MS);
    return () => {
      live = false;
      window.clearTimeout(t);
    };
    // `key` stands for `options` (deep compare)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [siteId, key]);

  if (state === 'unavailable' || !siteId) return null;
  if (state === 'error' && !est) {
    return <div className="faint" style={{ fontSize: 12.5 }}>The AI estimate is not available right now.</div>;
  }
  if (!est) return <div className="faint" style={{ fontSize: 12.5 }}>Estimating AI tokens…</div>;
  const remaining = (v: number | null | undefined) => (v === null || v === undefined ? 'unlimited' : fmtTokens(v));
  return (
    <div className="card ai-estimate-card" style={{ margin: '12px 0' }}>
      <div className="card-title" style={{ marginBottom: 6 }}>
        AI estimate {state === 'loading' && <span className="faint">(updating…)</span>}
      </div>
      {!est.ai_enabled ? (
        <div className="faint" style={{ fontSize: 13 }}>
          No AI calls planned: heuristics only. Turn on “Refine anchors with AI”, the Contextual strategy or Settings → AI
          Models → <b>Always use AI</b> to use the AI.
        </div>
      ) : (
        <>
          <div className="ai-estimate">
            <span>
              <b>{fmtTokens(est.tokens_total)}</b> tokens ({fmtTokens(est.tokens_in)} in / {fmtTokens(est.tokens_out)} out)
            </span>
            <span>
              <b>{est.calls.toLocaleString()}</b> calls
            </span>
            <span>
              <b>{fmtUsd(est.cost_usd)}</b> est. cost
            </span>
            <span>
              {est.items_estimated.toLocaleString()} links considered
              {est.ai_items
                ? ` · ${est.ai_items.anchor_pick ?? 0} anchor picks, ${est.ai_items.relevance_check ?? 0} relevance checks, ${est.ai_items.contextual ?? 0} sentences`
                : ''}
            </span>
          </div>
          <div className="faint" style={{ fontSize: 12.5, marginTop: 6 }}>
            Budget left today: low {remaining(est.budget?.low)} · main {remaining(est.budget?.main)} · high{' '}
            {remaining(est.budget?.high)} · this site {remaining(est.budget?.site)}
            {est.always_on ? ' · Always use AI is on' : ''}
            {est.basis === 'defaults' ? ' · based on default averages (no history yet)' : est.basis ? ' · based on recent usage' : ''}
          </div>
          {!est.within_budget && (
            <div className="ai-estimate-warn" style={{ fontSize: 13, marginTop: 6 }}>
              This run may use more than today&apos;s remaining budget. When the budget runs out the rest is linked with
              heuristics only.
            </div>
          )}
          {est.note && <div className="faint" style={{ fontSize: 12.5, marginTop: 4 }}>{est.note}</div>}
        </>
      )}
    </div>
  );
}

/** AI usage of a run (Results). Renders nothing when the server sends no `ai`. */
export function AiSummaryCard({ ai }: { ai: LinkRunAiSummary | null | undefined }) {
  if (!ai) return null;
  const calls = ai.calls ?? 0;
  const hits = ai.cache_hits ?? 0;
  if (calls === 0 && hits === 0 && !ai.fallback && !ai.asked && !ai.skipped_confident) return null;
  const tasks = Object.entries(ai.by_task ?? {});
  return (
    <div className="card" style={{ margin: '12px 0' }}>
      <div className="card-title" style={{ marginBottom: 6 }}>
        AI summary
      </div>
      <div className="ai-estimate">
        <span>
          <b>{calls.toLocaleString()}</b> calls
        </span>
        <span>
          <b>{fmtTokens(ai.tokens_in)}</b> in / <b>{fmtTokens(ai.tokens_out)}</b> out tokens
          {(ai.estimated ?? 0) > 0 ? ' (partly estimated)' : ''}
        </span>
        <span>
          <b>{fmtUsd(ai.cost_usd)}</b>
        </span>
        <span>
          <b>{hits.toLocaleString()}</b> cache hits
        </span>
        {ai.skipped_confident !== undefined && (
          <span>
            <b>{ai.skipped_confident.toLocaleString()}</b> skipped (heuristic confident)
          </span>
        )}
        {(ai.errors ?? 0) > 0 && (
          <span className="lr-text-bad">
            <b>{ai.errors}</b> errors
          </span>
        )}
      </div>
      {tasks.length > 0 && (
        <div className="faint" style={{ fontSize: 12.5, marginTop: 6 }}>
          {tasks.map(([k, v]) => `${k}: ${v.calls ?? 0} calls, ${fmtTokens((v.tokens_in ?? 0) + (v.tokens_out ?? 0))} tokens`).join(' · ')}
        </div>
      )}
      {ai.fallback && (
        <div className="ai-estimate-warn" style={{ fontSize: 13, marginTop: 6 }}>
          Fell back to heuristics: {ai.fallback}.
        </div>
      )}
    </div>
  );
}
