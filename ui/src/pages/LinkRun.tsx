// Link Run (v2.2, docs/specs/v2.2-link-runs.md §7): a 9-step guided bulk-linking flow.
//   1 Crawl → 2 Opportunities → 3 Strategies → 4 Linking mode → 5 Run → 6 Review →
//   7 Deploy → 8 Results → 9 Fix
// Built against the v2.2 contract; a 404/503 from the server shows a notice instead of an error.
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Link, useParams, useSearchParams } from 'react-router-dom';
import { ApiError, fetchJobRun, triggerCrawl } from '../api';
import {
  createLinkRun,
  fetchLinkRun,
  fetchLinkRunGroups,
  fetchOpportunities,
  isUnavailable,
  linkRunAction,
  patchLinkRunItem,
  selectLinkRunItems,
  v2ErrorMessage,
  type LinkRunAction,
} from '../api2';
import { Modal } from '../components/Modal';
import { PostLink } from '../components/PostLink';
import { EmptyState, Skeleton } from '../components/Skeleton';
import { FeatureUnavailable } from '../components/V2Notice';
import {
  CrawlChoice,
  MODE_LABEL,
  ModeDiagram,
  ModePicker,
  RunSwitches,
  STRATEGY_DEFS,
  STRATEGY_LABEL,
  StrategiesForm,
  normalizeOptions,
} from '../components/linkrun/OptionsForm';
import { RunsHistory } from '../components/linkrun/RunsHistory';
import { AiEstimateCard, AiSummaryCard } from '../components/linkrun/AiEstimate';
import {
  DEPLOYED_STATUSES,
  ItemBadge,
  RunBadge,
  isActive,
  n,
  needsFix,
  openUrl,
  stepForRun,
} from '../components/linkrun/runStatus';
import { fmtDate } from '../format';
import { useSites } from '../sites';
import { useToast } from '../toast';
import type { JobRun } from '../types';
import type { LinkRun, LinkRunItem, LinkRunOptions, LinkRunPostGroup, Opportunities } from '../types2';

const STEPS = [
  { n: 1, label: 'Crawl' },
  { n: 2, label: 'Opportunities' },
  { n: 3, label: 'Strategies' },
  { n: 4, label: 'Linking mode' },
  { n: 5, label: 'Run' },
  { n: 6, label: 'Review' },
  { n: 7, label: 'Deploy' },
  { n: 8, label: 'Results' },
  { n: 9, label: 'Fix' },
];

const POLL_MS = 2500;
const ITEMS_PAGE = 1000;

const OPP_DEFS: Record<string, { label: string; line: string; strategy?: string }> = {
  orphans: { label: 'Orphan posts', line: 'No contextual link points to them.', strategy: 'orphan_rescue' },
  under_linked: { label: 'Under-linked posts', line: 'Fewer inbound links than your minimum.', strategy: 'orphan_rescue' },
  semantic_pairs: { label: 'Semantic pairs', line: 'Paragraph and post pairs above the score threshold.', strategy: 'semantic' },
  keyword_rule_hits: { label: 'Keyword rule hits', line: 'Places where a keyword rule could link.', strategy: 'keyword_rules' },
  broken_internal: { label: 'Broken internal links', line: 'Links to dead internal URLs that can be swapped.', strategy: 'fix_broken' },
  silo_gaps: { label: 'Silo gaps', line: 'Links the silo shape needs but the site lacks.', strategy: 'silo' },
};

function humanKey(k: string): string {
  const s = k.replace(/_/g, ' ');
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function optsKey(siteId: string): string {
  return `sf_linkrun_opts:${siteId}`;
}
function readOpts(siteId: string): LinkRunOptions {
  try {
    const raw = localStorage.getItem(optsKey(siteId));
    return normalizeOptions(raw ? JSON.parse(raw) : null);
  } catch {
    return normalizeOptions(null);
  }
}
function writeOpts(siteId: string, o: LinkRunOptions): void {
  try {
    localStorage.setItem(optsKey(siteId), JSON.stringify(o));
  } catch {
    /* storage unavailable */
  }
}

function crawlAge(o: Opportunities | null): string {
  if (!o) return 'unknown';
  let h = o.last_crawl_age_hours ?? null;
  if (h === null && o.last_crawl_at) {
    const t = new Date(o.last_crawl_at).getTime();
    if (!Number.isNaN(t)) h = (Date.now() - t) / 3_600_000;
  }
  if (h === null) return 'never';
  if (h < 1) return `${Math.max(1, Math.round(h * 60))} min ago`;
  if (h < 48) return `${Math.round(h)} h ago`;
  return `${Math.round(h / 24)} days ago`;
}

function jobPct(job: JobRun | null): number {
  const p = (job?.stats as { progress?: { pct?: unknown } } | undefined)?.progress;
  return typeof p?.pct === 'number' ? Math.max(0, Math.min(100, p.pct)) : 0;
}

function pctOf(run: LinkRun | null): number {
  const p = run?.progress?.pct;
  return typeof p === 'number' ? Math.max(0, Math.min(100, Math.round(p))) : 0;
}

function errMsg(e: unknown, fallback: string): string {
  if (e instanceof ApiError && e.status === 409) return `${e.message} (the run is not in the right state for this)`;
  return v2ErrorMessage(e, fallback);
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function LinkRunPage() {
  const { siteId = '' } = useParams<{ siteId: string }>();
  const [params, setParams] = useSearchParams();
  const runParam = params.get('run');
  const view = params.get('view') === 'history' ? 'history' : 'run';
  const { sites } = useSites();
  const site = sites.find((s) => s.id === siteId);
  const toast = useToast();

  const [options, setOptionsRaw] = useState<LinkRunOptions>(() => readOpts(siteId));
  const setOptions = useCallback(
    (o: LinkRunOptions) => {
      setOptionsRaw(o);
      writeOpts(siteId, o);
    },
    [siteId],
  );
  const [name, setName] = useState('');
  const [step, setStep] = useState(1);
  const [run, setRun] = useState<LinkRun | null>(null);
  const [runError, setRunError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [busy, setBusy] = useState(false);
  const [historyKey, setHistoryKey] = useState(0);

  // opportunities
  const [opps, setOpps] = useState<Opportunities | null>(null);
  const [oppsState, setOppsState] = useState<'loading' | 'ok' | 'unavailable' | 'error'>('loading');
  const loadOpps = useCallback(async () => {
    setOppsState('loading');
    try {
      setOpps(await fetchOpportunities(siteId));
      setOppsState('ok');
    } catch (e) {
      setOppsState(isUnavailable(e) ? 'unavailable' : 'error');
    }
  }, [siteId]);
  useEffect(() => {
    void loadOpps();
  }, [loadOpps]);

  // reset when the site changes
  useEffect(() => {
    setOptionsRaw(readOpts(siteId));
    setRun(null);
    setStep(1);
  }, [siteId]);

  // load ?run=
  useEffect(() => {
    if (!runParam) return;
    let live = true;
    setRunError(null);
    fetchLinkRun(runParam)
      .then((r) => {
        if (!live) return;
        setRun(r);
        if (r.options) setOptionsRaw(normalizeOptions(r.options));
        setStep(stepForRun(r));
      })
      .catch((e) => {
        if (!live) return;
        if (isUnavailable(e)) setUnavailable(true);
        else setRunError(v2ErrorMessage(e, 'Could not load this run'));
      });
    return () => {
      live = false;
    };
  }, [runParam]);

  // poll while the run is busy
  const lastStatus = useRef<string | null>(null);
  // set while Results/Fix run verify/repair/undo: the run passes through `deploying`, but the
  // page must stay on that step (E2E 2.2: "Apply properly" jumped to Deploy, the Fix step
  // unmounted and its "Repair finished" never came)
  const stay = useRef(false);
  const onRunAction = useCallback((r: LinkRun) => {
    stay.current = true;
    setRun(r);
  }, []);
  useEffect(() => {
    if (!run || !isActive(run)) return;
    const id = run.id;
    const t = window.setInterval(() => {
      fetchLinkRun(id)
        .then(setRun)
        .catch(() => undefined);
    }, POLL_MS);
    return () => window.clearInterval(t);
  }, [run]);

  // advance the stepper when the run changes status
  useEffect(() => {
    if (!run) {
      lastStatus.current = null;
      return;
    }
    if (lastStatus.current !== null && lastStatus.current !== run.status) {
      const staying = stay.current;
      if (staying && !isActive(run)) stay.current = false;
      if (!staying) setStep(stepForRun(run));
      if (run.status === 'ready') toast.push(`Run ready: ${n(run.stats?.selected ?? run.stats?.opportunities)} links to review`, 'success');
      if (DEPLOYED_STATUSES.has(run.status) && !staying) toast.push('Deploy finished', 'success');
      if (run.status === 'failed') toast.push(`Run failed${run.error ? `: ${run.error}` : ''}`, 'error');
      setHistoryKey((k) => k + 1);
    }
    lastStatus.current = run.status;
  }, [run, toast]);

  const openRun = (id: string) => {
    stay.current = false;
    setParams({ run: id });
  };
  const newRun = () => {
    stay.current = false;
    setRun(null);
    setRunError(null);
    setStep(1);
    setParams({});
  };

  const start = async () => {
    if (!Object.values(options.strategies).some(Boolean)) {
      setStep(3);
      toast.push('Tick at least one strategy first', 'error');
      return;
    }
    setBusy(true);
    setRunError(null);
    try {
      const r = await createLinkRun(siteId, options, name.trim() || undefined);
      setRun(r);
      lastStatus.current = r.status;
      setParams({ run: r.id }, { replace: true });
      setStep(stepForRun(r));
      setHistoryKey((k) => k + 1);
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setRunError(errMsg(e, 'Could not start the run'));
    } finally {
      setBusy(false);
    }
  };

  const act = async (action: LinkRunAction, okMsg: string): Promise<LinkRun | null> => {
    if (!run) return null;
    setBusy(true);
    try {
      const r = await linkRunAction(run.id, action);
      const next = r ?? (await fetchLinkRun(run.id).catch(() => run));
      setRun(next);
      toast.push(okMsg, 'success');
      return next;
    } catch (e) {
      toast.push(errMsg(e, `${action} failed`), 'error');
      return null;
    } finally {
      setBusy(false);
    }
  };

  // max step the rail lets you open
  // after a deploy, Fix (9) is reachable from the rail too, not only via "Fix them"
  const reach = run ? Math.max(stepForRun(run) === 8 ? 9 : stepForRun(run), step) : 5;
  const configLocked = !!run;

  return (
    <div className="lr-page">
      <div className="page-head">
        <div>
          <h1>Link Run</h1>
          <div className="page-sub">
            Plan, review and deploy internal links in one batch{site ? ` on ${site.domain}` : ''}, then check each one on the live page.
          </div>
        </div>
        <div className="spacer" />
        <Link className="btn" to="/settings?section=linking" data-hint="Schedule link runs for this site in Settings → Linking Rules.">
          Auto-Linker
        </Link>
        {run && (
          <button className="btn btn-primary" onClick={newRun} disabled={isActive(run)}>
            New Link Run
          </button>
        )}
      </div>

      <div className="tabs">
        <button className={`tab${view === 'run' ? ' active' : ''}`} onClick={() => setParams(run ? { run: run.id } : {})}>
          {run ? 'This run' : 'New run'}
        </button>
        <button
          className={`tab${view === 'history' ? ' active' : ''}`}
          onClick={() => setParams(run ? { run: run.id, view: 'history' } : { view: 'history' })}
        >
          History
        </button>
      </div>

      {unavailable ? (
        <FeatureUnavailable feature="Link Runs (SiloForge 2.2)" />
      ) : view === 'history' ? (
        <RunsHistory siteId={siteId} onOpen={openRun} refreshKey={historyKey} />
      ) : (
        <div className="lr-shell">
          <ol className="lr-rail" aria-label="Link run steps">
            {STEPS.map((s) => {
              const locked = s.n > reach || (configLocked && s.n <= 4);
              const state = s.n === step ? 'active' : s.n < step || (run && s.n < stepForRun(run)) ? 'done' : '';
              return (
                <li key={s.n}>
                  <button
                    type="button"
                    className={`lr-rail-step ${state}`}
                    disabled={locked && s.n !== step}
                    aria-current={s.n === step ? 'step' : undefined}
                    onClick={() => setStep(s.n)}
                  >
                    <span className="lr-rail-num">{s.n}</span>
                    <span className="lr-rail-label">{s.label}</span>
                    <span className="lr-rail-note">{railNote(s.n, { options, opps, run })}</span>
                  </button>
                </li>
              );
            })}
          </ol>

          <section className="lr-panel" aria-live="polite">
            {runError && <div className="error-box">{runError}</div>}
            {configLocked && step <= 4 && (
              <div className="info-note">
                <span className="info-note-icon">i</span>
                <span className="info-note-text">
                  These settings belong to the run that is open. Start a <b>New Link Run</b> to change them.
                </span>
              </div>
            )}

            {step === 1 && (
              <StepCrawl
                siteId={siteId}
                opps={opps}
                oppsState={oppsState}
                value={options}
                onChange={setOptions}
                onCrawled={() => void loadOpps()}
                onNext={() => setStep(2)}
                locked={configLocked}
              />
            )}
            {step === 2 && <StepOpportunities opps={opps} state={oppsState} reload={() => void loadOpps()} onNext={() => setStep(3)} />}
            {step === 3 && (
              <StepFrame title="Pick the strategies" lead="Each strategy finds links in its own way. Tick as many as you like." onNext={() => setStep(4)}>
                <fieldset disabled={configLocked} className="lr-fieldset">
                  <StrategiesForm value={options} onChange={setOptions} siteId={siteId} counts={opps?.by_strategy} />
                </fieldset>
              </StepFrame>
            )}
            {step === 4 && (
              <StepFrame title="Choose how to link" lead="The shape the new links take across the site." onNext={() => setStep(5)}>
                <fieldset disabled={configLocked} className="lr-fieldset">
                  <ModePicker value={options.mode} onChange={(m) => setOptions({ ...options, mode: m })} />
                  <RunSwitches value={options} onChange={setOptions} />
                </fieldset>
              </StepFrame>
            )}
            {step === 5 && (
              <StepRun siteId={siteId ?? null} run={run} options={options} name={name} setName={setName} busy={busy} onStart={() => void start()} onCancel={() => void act('cancel', 'Run cancelled')} onReview={() => setStep(6)} />
            )}
            {step === 6 && run && (
              <StepReview
                run={run}
                busy={busy}
                onDeploy={async () => {
                  const r = await act('deploy', 'Deploy started');
                  if (r) setStep(stepForRun(r.status === 'ready' ? { ...r, status: 'deploying' } : r));
                }}
              />
            )}
            {step === 7 && run && <StepDeploy run={run} busy={busy} onCancel={() => void act('cancel', 'Deploy cancelled')} onResults={() => setStep(8)} />}
            {step === 8 && run && <StepResults run={run} onRun={onRunAction} onFix={() => setStep(9)} />}
            {step === 9 && run && <StepFix run={run} onRun={onRunAction} onResults={() => setStep(8)} />}
            {step >= 6 && !run && <EmptyState title="No run open" hint="Start a run in step 5, or open one from History." />}
          </section>
        </div>
      )}
    </div>
  );
}

function railNote(stepN: number, ctx: { options: LinkRunOptions; opps: Opportunities | null; run: LinkRun | null }): string {
  const { options, opps, run } = ctx;
  const s = run?.stats ?? {};
  switch (stepN) {
    case 1:
      return options.crawl === 'skip' ? 'Use last crawl' : options.crawl === 'always' ? 'Crawl first' : 'Crawl if stale';
    case 2:
      return opps ? `${opps.counts.reduce((a, c) => a + c.count, 0).toLocaleString()} found` : '';
    case 3: {
      const k = Object.values(options.strategies).filter(Boolean).length;
      return `${k} ticked`;
    }
    case 4:
      return MODE_LABEL[options.mode] ?? options.mode;
    case 5:
      return run ? (isActive(run) && run.status !== 'deploying' ? `${pctOf(run)}%` : '') : 'Not started';
    case 6:
      return run && (run.status === 'ready' || n(s.selected) > 0) ? `${n(s.selected)} selected` : '';
    case 7:
      return run?.status === 'deploying' ? `${pctOf(run)}%` : '';
    case 8:
      return run && (DEPLOYED_STATUSES.has(run.status) || n(s.applied) > 0) ? `${n(s.applied)} applied` : '';
    case 9: {
      const bad = n(s.failed) + n(s.not_present);
      return run && bad > 0 ? `${bad} to fix` : '';
    }
    default:
      return '';
  }
}

function StepFrame({
  title,
  lead,
  children,
  onNext,
  nextLabel = 'Continue',
  actions,
}: {
  title: string;
  lead?: string;
  children: ReactNode;
  onNext?: () => void;
  nextLabel?: string;
  actions?: ReactNode;
}) {
  return (
    <div className="lr-step">
      <h2 className="lr-step-title">{title}</h2>
      {lead && <p className="lr-step-lead">{lead}</p>}
      {children}
      {(onNext || actions) && (
        <div className="lr-step-foot">
          {actions}
          <div className="spacer" />
          {onNext && (
            <button className="btn btn-primary" onClick={onNext}>
              {nextLabel}
            </button>
          )}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 1 Crawl
// ---------------------------------------------------------------------------

function StepCrawl({
  siteId,
  opps,
  oppsState,
  value,
  onChange,
  onCrawled,
  onNext,
  locked,
}: {
  siteId: string;
  opps: Opportunities | null;
  oppsState: string;
  value: LinkRunOptions;
  onChange: (o: LinkRunOptions) => void;
  onCrawled: () => void;
  onNext: () => void;
  locked: boolean;
}) {
  const toast = useToast();
  const [job, setJob] = useState<JobRun | null>(null);
  const [crawlBusy, setCrawlBusy] = useState(false);
  const running = job !== null && (job.status === 'queued' || job.status === 'running');

  useEffect(() => {
    if (!job || !running) return;
    const t = window.setInterval(() => {
      fetchJobRun(job.id)
        .then((j) => {
          setJob(j);
          if (j.status === 'done') {
            toast.push('Crawl finished', 'success');
            onChange({ ...value, crawl: 'skip' });
            onCrawled();
          } else if (j.status === 'failed') toast.push(`Crawl failed${j.error ? `: ${j.error}` : ''}`, 'error');
        })
        .catch(() => undefined);
    }, 3000);
    return () => window.clearInterval(t);
  }, [job, running, toast, onChange, value, onCrawled]);

  const crawlNow = async () => {
    setCrawlBusy(true);
    try {
      const { job_run_id } = await triggerCrawl(siteId);
      setJob({
        id: job_run_id,
        site_id: siteId,
        kind: 'crawl',
        status: 'queued',
        stats: {},
        error: null,
        started_at: null,
        finished_at: null,
        created_at: new Date().toISOString(),
      });
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Crawl failed to start', 'error');
    } finally {
      setCrawlBusy(false);
    }
  };

  return (
    <StepFrame
      title="Start from fresh content"
      lead="Link runs plan from the crawled copy of your posts. Crawl now, or reuse the last crawl."
      onNext={onNext}
    >
      <div className="lr-crawl-status">
        <div>
          <div className="lr-big">{oppsState === 'loading' ? '…' : crawlAge(opps)}</div>
          <div className="muted">last crawl</div>
        </div>
        <div>
          <div className="lr-big">{opps?.posts_total?.toLocaleString() ?? '—'}</div>
          <div className="muted">posts indexed</div>
        </div>
        <div className="spacer" />
        <button className="btn" onClick={() => void crawlNow()} disabled={crawlBusy || running || locked}>
          {running ? 'Crawling…' : 'Crawl now'}
        </button>
        <button
          className="btn"
          disabled={locked}
          onClick={() => {
            onChange({ ...value, crawl: 'skip' });
            onNext();
          }}
        >
          Use last crawl
        </button>
      </div>
      {running && (
        <div className="lr-bar" aria-label="Crawl progress">
          <div className="lr-bar-fill" style={{ width: `${Math.max(4, jobPct(job))}%` }} />
        </div>
      )}
      <div className="lr-field-label">When the run starts</div>
      <fieldset disabled={locked} className="lr-fieldset">
        <CrawlChoice value={value.crawl} onChange={(c) => onChange({ ...value, crawl: c })} />
      </fieldset>
    </StepFrame>
  );
}

// ---------------------------------------------------------------------------
// 2 Opportunities
// ---------------------------------------------------------------------------

function StepOpportunities({
  opps,
  state,
  reload,
  onNext,
}: {
  opps: Opportunities | null;
  state: string;
  reload: () => void;
  onNext: () => void;
}) {
  return (
    <StepFrame
      title="What can be linked"
      lead="Everything SiloForge found on the last crawl. Each card feeds one of the strategies on the next step."
      onNext={onNext}
      actions={
        <button className="btn" onClick={reload}>
          Refresh
        </button>
      }
    >
      {state === 'loading' && <Skeleton rows={4} />}
      {state === 'unavailable' && (
        <div className="info-note">
          <span className="info-note-icon">i</span>
          <span className="info-note-text">This server does not report opportunities yet. You can still pick strategies and run.</span>
        </div>
      )}
      {state === 'error' && <div className="error-box">Could not load opportunities. Try Refresh.</div>}
      {state === 'ok' && opps && (
        opps.counts.length === 0 ? (
          <EmptyState title="Nothing found yet" hint="Crawl the site first, then refresh this step." />
        ) : (
          <div className="lr-opps">
            {opps.counts.map((c) => {
              const d = OPP_DEFS[c.key];
              return (
                <div key={c.key} className={`lr-opp${c.count === 0 ? ' zero' : ''}`}>
                  <div className="lr-opp-count">{c.count.toLocaleString()}</div>
                  <div className="lr-opp-label">{d?.label ?? humanKey(c.key)}</div>
                  {d && <div className="lr-line">{d.line}</div>}
                  {d?.strategy && <div className="lr-opp-feeds">{STRATEGY_LABEL[d.strategy]}</div>}
                </div>
              );
            })}
          </div>
        )
      )}
    </StepFrame>
  );
}

// ---------------------------------------------------------------------------
// 5 Run
// ---------------------------------------------------------------------------

const RUN_PHASES = [
  { key: 'crawling', label: 'Crawl' },
  { key: 'planning', label: 'Plan and score' },
  { key: 'ready', label: 'Ready' },
];

function StepRun({
  siteId,
  run,
  options,
  name,
  setName,
  busy,
  onStart,
  onCancel,
  onReview,
}: {
  siteId: string | null;
  run: LinkRun | null;
  options: LinkRunOptions;
  name: string;
  setName: (s: string) => void;
  busy: boolean;
  onStart: () => void;
  onCancel: () => void;
  onReview: () => void;
}) {
  if (!run) {
    const ticked = STRATEGY_DEFS.filter((s) => options.strategies[s.key]).map((s) => s.label);
    return (
      <StepFrame title="Run the plan" lead="Nothing is written to your site yet. The run stops at a preview you review first.">
        <dl className="lr-summary">
          <dt>Crawl</dt>
          <dd>{options.crawl === 'skip' ? 'Use last crawl' : options.crawl === 'always' ? 'Crawl first' : 'Crawl if older than 24 h'}</dd>
          <dt>Strategies</dt>
          <dd>{ticked.length ? ticked.join(', ') : <span className="lr-text-bad">none ticked</span>}</dd>
          <dt>Mode</dt>
          <dd className="lr-summary-mode">
            <ModeDiagram mode={options.mode} />
            {MODE_LABEL[options.mode]}
          </dd>
          <dt>Limits</dt>
          <dd>
            score at least {options.filters.min_score.toFixed(2)}, up to {options.filters.max_new_per_post} new per post,{' '}
            {options.filters.max_total.toLocaleString()} in total
          </dd>
          <dt>AI</dt>
          <dd>
            {options.ai_refine ? 'refine anchors' : 'no anchor refinement'}
            {options.strategies.contextual ? ', contextual sentences' : ''}
            {options.high_priority ? ', high priority (stronger model)' : ''}
          </dd>
        </dl>
        <AiEstimateCard siteId={siteId} options={options} />
        <label className="field" style={{ maxWidth: 420 }}>
          Name (optional)
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. October orphan rescue" />
        </label>
        <div className="lr-step-foot">
          <div className="spacer" />
          <button className="btn btn-primary lr-cta" onClick={onStart} disabled={busy}>
            {busy ? 'Starting…' : 'Start run'}
          </button>
        </div>
      </StepFrame>
    );
  }
  const idx = run.status === 'crawling' ? 0 : run.status === 'planning' ? 1 : run.status === 'draft' ? -1 : 2;
  const failed = run.status === 'failed' || run.status === 'cancelled';
  return (
    <StepFrame title={failed ? 'The run stopped' : run.status === 'ready' ? 'The plan is ready' : 'Planning links…'}>
      <ol className="lr-phases">
        {RUN_PHASES.map((p, i) => (
          <li key={p.key} className={i < idx ? 'done' : i === idx ? (failed ? 'bad' : 'active') : ''}>
            <span className="lr-phase-dot" />
            {p.label}
          </li>
        ))}
      </ol>
      {isActive(run) && (
        <>
          <div className="lr-progress-label">
            <span>{run.progress?.message || run.progress?.phase || 'Working…'}</span>
            <span className="mono">{pctOf(run)}%</span>
          </div>
          <div className="lr-bar">
            <div className="lr-bar-fill" style={{ width: `${Math.max(3, pctOf(run))}%` }} />
          </div>
        </>
      )}
      {failed && <div className="error-box">{run.error || `This run was ${run.status}.`}</div>}
      <div className="lr-step-foot">
        {isActive(run) && (
          <button className="btn btn-danger" onClick={onCancel} disabled={busy}>
            Cancel run
          </button>
        )}
        <div className="spacer" />
        {run.status === 'ready' && (
          <button className="btn btn-primary" onClick={onReview}>
            Review {n(run.stats?.selected ?? run.stats?.opportunities)} links
          </button>
        )}
      </div>
    </StepFrame>
  );
}

// ---------------------------------------------------------------------------
// Items loader (review / results / fix)
// ---------------------------------------------------------------------------

function useRunItems(runId: string, refreshKey: unknown) {
  const [groups, setGroups] = useState<LinkRunPostGroup[] | null>(null);
  const [total, setTotal] = useState(0);
  const [totalPosts, setTotalPosts] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      const r = await fetchLinkRunGroups(runId, { limit: ITEMS_PAGE, offset: 0 });
      setGroups(r.posts);
      setTotal(r.total);
      setTotalPosts(r.totalPosts);
    } catch (e) {
      setError(v2ErrorMessage(e, 'Could not load the links of this run'));
      setGroups((g) => g ?? []);
    }
  }, [runId]);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);

  const loaded = groups?.reduce((a, g) => a + g.links.length, 0) ?? 0;
  // the server pages over posts (group=post), so the next page starts after the posts loaded
  const postsLoaded = groups?.length ?? 0;
  const hasMore = postsLoaded < totalPosts;
  const loadMore = async () => {
    setLoadingMore(true);
    try {
      const r = await fetchLinkRunGroups(runId, { limit: ITEMS_PAGE, offset: postsLoaded });
      setGroups((cur) => {
        const map = new Map((cur ?? []).map((g) => [g.post.id, { ...g, links: [...g.links] }]));
        for (const g of r.posts) {
          const ex = map.get(g.post.id);
          if (ex) ex.links.push(...g.links);
          else map.set(g.post.id, g);
        }
        return [...map.values()];
      });
    } catch (e) {
      setError(v2ErrorMessage(e, 'Could not load more'));
    } finally {
      setLoadingMore(false);
    }
  };

  const patchItem = (edgeId: string, patch: Partial<LinkRunItem>) =>
    setGroups((cur) =>
      cur ? cur.map((g) => ({ ...g, links: g.links.map((l) => (l.edge_id === edgeId ? { ...l, ...patch } : l)) })) : cur,
    );

  return { groups, setGroups, total, loaded, hasMore, error, load, loadMore, loadingMore, patchItem };
}

function ScoreMini({ score }: { score?: number | null }) {
  if (typeof score !== 'number') return null;
  const pct = Math.round(Math.max(0, Math.min(1, score)) * 100);
  return (
    <span className="lr-score" title={`Score ${score.toFixed(2)}`}>
      <span className="lr-score-track">
        <span className="lr-score-fill" style={{ width: `${pct}%` }} />
      </span>
      <span className="mono">{pct}</span>
    </span>
  );
}

// ---------------------------------------------------------------------------
// 6 Review
// ---------------------------------------------------------------------------

function StepReview({ run, busy, onDeploy }: { run: LinkRun; busy: boolean; onDeploy: () => Promise<void> }) {
  const toast = useToast();
  const { groups, setGroups, total, loaded, hasMore, error, loadMore, loadingMore, patchItem } = useRunItems(run.id, run.status);
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [q, setQ] = useState('');
  const [confirm, setConfirm] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState('');
  const editable = run.status === 'ready';

  const all = useMemo(() => groups?.flatMap((g) => g.links) ?? [], [groups]);
  const selectedCount = all.filter((l) => l.status === 'selected').length;
  const shown = useMemo(() => {
    const t = q.trim().toLowerCase();
    if (!groups || !t) return groups ?? [];
    return groups
      .map((g) => ({
        ...g,
        links: g.post.title.toLowerCase().includes(t)
          ? g.links
          : g.links.filter((l) => l.anchor.toLowerCase().includes(t) || l.target.title.toLowerCase().includes(t)),
      }))
      .filter((g) => g.links.length > 0);
  }, [groups, q]);

  const setStatus = (ids: Set<string>, sel: boolean) =>
    setGroups((cur) =>
      cur
        ? cur.map((g) => ({
            ...g,
            links: g.links.map((l) =>
              ids.has(l.edge_id) && (l.status === 'selected' || l.status === 'suggested')
                ? { ...l, status: sel ? 'selected' : 'suggested' }
                : l,
            ),
          }))
        : cur,
    );

  const toggle = async (items: LinkRunItem[], sel: boolean) => {
    const ids = new Set(items.map((i) => i.edge_id));
    setStatus(ids, sel);
    try {
      await selectLinkRunItems(run.id, { edge_ids: [...ids], deselect: !sel });
    } catch (e) {
      setStatus(ids, !sel);
      toast.push(errMsg(e, 'Could not change the selection'), 'error');
    }
  };

  const toggleAll = async (sel: boolean) => {
    const ids = new Set(all.map((i) => i.edge_id));
    const before = groups;
    setStatus(ids, sel);
    try {
      await selectLinkRunItems(run.id, sel ? { all: true } : { all: true, deselect: true });
    } catch (e) {
      setGroups(before);
      toast.push(errMsg(e, 'Could not change the selection'), 'error');
    }
  };

  const saveAnchor = async (item: LinkRunItem, anchor: string) => {
    const a = anchor.trim();
    setEditing(null);
    if (!a || a === item.anchor) return;
    const prev = item.anchor;
    patchItem(item.edge_id, { anchor: a, open_url: null });
    try {
      const updated = await patchLinkRunItem(run.id, item.edge_id, { anchor: a });
      if (updated) patchItem(item.edge_id, updated);
    } catch (e) {
      patchItem(item.edge_id, { anchor: prev });
      toast.push(errMsg(e, 'Could not change the anchor'), 'error');
    }
  };

  const toggleOpen = (id: string) =>
    setOpen((cur) => {
      const next = new Set(cur);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  if (groups === null) return <Skeleton rows={8} />;

  return (
    <div className="lr-step lr-review">
      <h2 className="lr-step-title">Review the links</h2>
      <p className="lr-step-lead">
        Untick anything you don’t want, fix an anchor, or pick an alternate. Only selected links are deployed.
      </p>
      {error && <div className="error-box">{error}</div>}
      <div className="table-toolbar">
        <span className="lr-counts">
          <b>{selectedCount.toLocaleString()}</b> selected of {total.toLocaleString()} across {groups.length.toLocaleString()} posts
        </span>
        <div className="spacer" />
        <input type="search" className="input" placeholder="Search posts, anchors, targets" value={q} onChange={(e) => setQ(e.target.value)} aria-label="Search" />
        <button className="btn btn-sm" disabled={!editable} onClick={() => void toggleAll(true)}>
          Select all
        </button>
        <button className="btn btn-sm" disabled={!editable} onClick={() => void toggleAll(false)}>
          Select none
        </button>
        <button className="btn btn-sm btn-ghost" onClick={() => setOpen(open.size ? new Set() : new Set(groups.map((g) => g.post.id)))}>
          {open.size ? 'Collapse all' : 'Expand all'}
        </button>
      </div>

      {shown.length === 0 ? (
        <EmptyState title={q ? 'No match' : 'This run found no links'} hint={q ? 'Try another search.' : 'Loosen the filters (lower the minimum score) and start a new run.'} />
      ) : (
        <ul className="lr-posts">
          {shown.map((g) => {
            const sel = g.links.filter((l) => l.status === 'selected').length;
            const isOpen = open.has(g.post.id);
            return (
              <li key={g.post.id} className={`lr-post${isOpen ? ' open' : ''}`}>
                <div className="lr-post-head">
                  <input
                    type="checkbox"
                    checked={sel === g.links.length && sel > 0}
                    ref={(el) => {
                      if (el) el.indeterminate = sel > 0 && sel < g.links.length;
                    }}
                    disabled={!editable}
                    onChange={(e) => void toggle(g.links, e.target.checked)}
                    aria-label={`Select every link in ${g.post.title}`}
                  />
                  <button type="button" className="lr-post-toggle" onClick={() => toggleOpen(g.post.id)} aria-expanded={isOpen}>
                    <span className="lr-caret" aria-hidden="true" />
                    <span className="lr-post-title">{g.post.title || g.post.url || '(untitled)'}</span>
                  </button>
                  <span className="lr-post-count">
                    {sel}/{g.links.length}
                  </span>
                </div>
                {isOpen && (
                  <ul className="lr-links">
                    {g.links.map((l) => (
                      <li key={l.edge_id} className={`lr-link${l.status === 'selected' ? '' : ' off'}`}>
                        <input
                          type="checkbox"
                          checked={l.status === 'selected'}
                          disabled={!editable}
                          onChange={(e) => void toggle([l], e.target.checked)}
                          aria-label={`Deploy link “${l.anchor}”`}
                        />
                        <div className="lr-link-main">
                          {editing === l.edge_id ? (
                            <input
                              className="input lr-anchor-input"
                              autoFocus
                              value={draft}
                              onChange={(e) => setDraft(e.target.value)}
                              onBlur={() => void saveAnchor(l, draft)}
                              onKeyDown={(e) => {
                                if (e.key === 'Enter') void saveAnchor(l, draft);
                                if (e.key === 'Escape') setEditing(null);
                              }}
                              aria-label="Anchor text"
                            />
                          ) : (
                            <span className="lr-anchor">“{l.anchor}”</span>
                          )}
                          <span className="lr-to">links to</span>
                          <PostLink url={l.target.url} title={l.target.title} />
                        </div>
                        <div className="lr-link-meta">
                          {l.strategy && <span className="chip chip-mode">{STRATEGY_LABEL[l.strategy] ?? l.strategy}</span>}
                          {l.anchor_type && <span className={`chip chip-type-${l.anchor_type}`}>{l.anchor_type}</span>}
                          <ScoreMini score={l.score} />
                        </div>
                        <div className="lr-link-actions">
                          {editable && (l.alternates?.length ?? 0) > 0 && (
                            <select
                              value=""
                              aria-label="Pick an alternate anchor"
                              onChange={(e) => e.target.value && void saveAnchor(l, e.target.value)}
                            >
                              <option value="">Alternates…</option>
                              {l.alternates?.map((a) => (
                                <option key={a} value={a}>
                                  {a}
                                </option>
                              ))}
                            </select>
                          )}
                          {editable && (
                            <button
                              className="btn btn-sm btn-ghost"
                              onClick={() => {
                                setDraft(l.anchor);
                                setEditing(l.edge_id);
                              }}
                            >
                              Edit anchor
                            </button>
                          )}
                        </div>
                      </li>
                    ))}
                  </ul>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {hasMore && (
        <button className="btn" onClick={() => void loadMore()} disabled={loadingMore}>
          {loadingMore ? 'Loading…' : `Load more (${(total - loaded).toLocaleString()} left)`}
        </button>
      )}

      <div className="lr-sticky">
        <span className="muted">
          {editable ? 'Links go live on WordPress when you deploy. Every one can be undone.' : <RunBadge status={run.status} />}
        </span>
        <div className="spacer" />
        <button className="btn btn-primary lr-cta" disabled={!editable || selectedCount === 0 || busy} onClick={() => setConfirm(true)}>
          Deploy {selectedCount.toLocaleString()} links
        </button>
      </div>

      {confirm && (
        <Modal
          title={`Deploy ${selectedCount.toLocaleString()} links?`}
          onClose={() => setConfirm(false)}
          footer={
            <>
              <button className="btn" onClick={() => setConfirm(false)}>
                Back to review
              </button>
              <button
                className="btn btn-primary"
                disabled={busy}
                onClick={() => {
                  setConfirm(false);
                  void onDeploy();
                }}
              >
                Deploy {selectedCount.toLocaleString()} links
              </button>
            </>
          }
        >
          <p>
            SiloForge writes {selectedCount.toLocaleString()} links into{' '}
            {groups.filter((g) => g.links.some((l) => l.status === 'selected')).length.toLocaleString()} live posts on WordPress
            {run.options?.verify === false ? '.' : ', then opens each page to confirm the link is there.'}
          </p>
          <p className="muted">You can undo the whole run later from History.</p>
        </Modal>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 7 Deploy
// ---------------------------------------------------------------------------

function StepDeploy({ run, busy, onCancel, onResults }: { run: LinkRun; busy: boolean; onCancel: () => void; onResults: () => void }) {
  const s = run.stats ?? {};
  const deploying = run.status === 'deploying';
  return (
    <StepFrame title={deploying ? 'Deploying links…' : 'Deploy finished'}>
      <div className="lr-progress-label">
        <span>{run.progress?.message || run.progress?.phase || (deploying ? 'Writing links to WordPress' : 'Done')}</span>
        <span className="mono">{deploying ? pctOf(run) : 100}%</span>
      </div>
      <div className="lr-bar">
        <div className={`lr-bar-fill${deploying ? ' live' : ''}`} style={{ width: `${deploying ? Math.max(3, pctOf(run)) : 100}%` }} />
      </div>
      <div className="lr-tally">
        <Tally label="Applied" value={n(s.applied)} tone="good" />
        <Tally label="Failed" value={n(s.failed)} tone="bad" />
        <Tally label="Not present" value={n(s.not_present)} tone="warn" />
        <Tally label="Selected" value={n(s.selected)} />
      </div>
      <div className="lr-step-foot">
        {deploying && (
          <button className="btn btn-danger" onClick={onCancel} disabled={busy}>
            Stop deploy
          </button>
        )}
        <div className="spacer" />
        {!deploying && (
          <button className="btn btn-primary" onClick={onResults}>
            See results
          </button>
        )}
      </div>
    </StepFrame>
  );
}

function Tally({ label, value, tone }: { label: string; value: number; tone?: 'good' | 'bad' | 'warn' }) {
  return (
    <div className={`lr-tally-item${tone ? ` lr-tone-${tone}` : ''}`}>
      <span className="lr-tally-num">{value.toLocaleString()}</span>
      <span className="lr-tally-label">{label}</span>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 8 Results
// ---------------------------------------------------------------------------

type ResultFilter = 'all' | 'applied' | 'failed' | 'not_present' | 'skipped';

function statusKey(l: LinkRunItem): string {
  return l.status === 'applied' && l.verify?.present === false ? 'not_present' : l.status;
}

/** Poll a job (if the server named one) until it ends, then call done. Without a job id, wait briefly. */
function useAfterJob() {
  const timer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (timer.current) window.clearInterval(timer.current);
    },
    [],
  );
  return useCallback((jobId: string | null | undefined, done: () => void) => {
    if (timer.current) window.clearInterval(timer.current);
    const started = Date.now();
    let polls = 0;
    timer.current = window.setInterval(() => {
      polls += 1;
      if (Date.now() - started > 15 * 60 * 1000) {
        if (timer.current) window.clearInterval(timer.current);
        done();
        return;
      }
      if (!jobId) {
        if (polls >= 3) {
          if (timer.current) window.clearInterval(timer.current);
          done();
        }
        return;
      }
      fetchJobRun(jobId)
        .then((j) => {
          if (j.status === 'done' || j.status === 'failed' || j.status === 'cancelled') {
            if (timer.current) window.clearInterval(timer.current);
            done();
          }
        })
        .catch(() => undefined);
    }, POLL_MS);
  }, []);
}

function StepResults({ run, onRun, onFix }: { run: LinkRun; onRun: (r: LinkRun) => void; onFix: () => void }) {
  const toast = useToast();
  const [refresh, setRefresh] = useState(0);
  const { groups, total, loaded, hasMore, error, loadMore, loadingMore } = useRunItems(run.id, `${run.status}-${refresh}`);
  const [filter, setFilter] = useState<ResultFilter>('all');
  const [verifying, setVerifying] = useState(false);
  const [undo, setUndo] = useState(false);
  const [busy, setBusy] = useState(false);
  const [open, setOpen] = useState<Set<string> | 'all'>('all');
  const afterJob = useAfterJob();

  // Results are about what was deployed: unticked suggestions stay in Review (E2E 2.2: 592
  // "Not selected" rows buried the 37 deployed links).
  const deployedGroups = useMemo(
    () => (groups ?? []).map((g) => ({ ...g, links: g.links.filter((l) => l.status !== 'suggested') })).filter((g) => g.links.length > 0),
    [groups],
  );
  const all = useMemo(() => deployedGroups.flatMap((g) => g.links), [deployedGroups]);
  const counts = useMemo(() => {
    const c: Record<string, number> = { applied: 0, failed: 0, not_present: 0, skipped: 0 };
    for (const l of all) {
      const k = statusKey(l);
      c[k] = (c[k] ?? 0) + 1;
    }
    return c;
  }, [all]);
  const fixable = all.filter(needsFix).length;
  const shown = useMemo(
    () =>
      deployedGroups
        .map((g) => ({ ...g, links: filter === 'all' ? g.links : g.links.filter((l) => statusKey(l) === filter) }))
        .filter((g) => g.links.length > 0),
    [deployedGroups, filter],
  );

  const verify = async () => {
    setVerifying(true);
    try {
      const r = await linkRunAction(run.id, 'verify');
      toast.push('Checking every applied link on the live pages…', 'info');
      afterJob((r as { verify_job_id?: string } | null)?.verify_job_id ?? null, () => {
        setVerifying(false);
        fetchLinkRun(run.id).then(onRun).catch(() => undefined);
        setRefresh((x) => x + 1);
        toast.push('Verify finished', 'success');
      });
    } catch (e) {
      setVerifying(false);
      toast.push(errMsg(e, 'Verify failed'), 'error');
    }
  };

  const doUndo = async () => {
    setBusy(true);
    try {
      const r = await linkRunAction(run.id, 'undo');
      setUndo(false);
      toast.push('Undo started', 'success');
      onRun(r ?? (await fetchLinkRun(run.id).catch(() => run)));
      afterJob(null, () => setRefresh((x) => x + 1));
    } catch (e) {
      toast.push(errMsg(e, 'Undo failed'), 'error');
    } finally {
      setBusy(false);
    }
  };

  const isOpen = (id: string) => open === 'all' || open.has(id);
  const toggleOpen = (id: string) =>
    setOpen((cur) => {
      const base = cur === 'all' ? new Set((groups ?? []).map((g) => g.post.id)) : new Set(cur);
      if (base.has(id)) base.delete(id);
      else base.add(id);
      return base;
    });

  if (groups === null) return <Skeleton rows={8} />;
  const FILTERS: Array<{ key: ResultFilter; label: string; count: number }> = [
    { key: 'all', label: 'All', count: all.length },
    { key: 'applied', label: 'Applied', count: counts.applied ?? 0 },
    { key: 'failed', label: 'Failed', count: counts.failed ?? 0 },
    { key: 'not_present', label: 'Not present', count: counts.not_present ?? 0 },
    { key: 'skipped', label: 'Skipped', count: counts.skipped ?? 0 },
  ];

  return (
    <div className="lr-step">
      <div className="lr-results-head">
        <div>
          <h2 className="lr-step-title">Results</h2>
          <p className="lr-step-lead">
            Open any link to see it highlighted on the live page. <RunBadge status={run.status} />
            {run.deployed_at && <span className="faint"> deployed {fmtDate(run.deployed_at)}</span>}
          </p>
        </div>
        <div className="spacer" />
        <button className="btn" onClick={() => void verify()} disabled={verifying || run.status === 'undone'}>
          {verifying ? 'Verifying…' : 'Verify again'}
        </button>
        {DEPLOYED_STATUSES.has(run.status) && (
          <button className="btn btn-danger" onClick={() => setUndo(true)}>
            Undo run
          </button>
        )}
      </div>

      <AiSummaryCard ai={run.ai} />
      <div className="lr-tally">
        <Tally label="Applied" value={counts.applied ?? n(run.stats?.applied)} tone="good" />
        <Tally label="Failed" value={counts.failed ?? 0} tone="bad" />
        <Tally label="Not present" value={counts.not_present ?? 0} tone="warn" />
        <Tally label="Skipped" value={counts.skipped ?? 0} />
        {n(run.stats?.repaired) > 0 && <Tally label="Repaired" value={n(run.stats?.repaired)} tone="good" />}
      </div>

      {fixable > 0 && (
        <div className="lr-fix-callout">
          <span>
            <b>{fixable.toLocaleString()}</b> {fixable === 1 ? 'link is' : 'links are'} not on the live page yet.
          </span>
          <div className="spacer" />
          <button className="btn btn-primary" onClick={onFix}>
            Fix them
          </button>
        </div>
      )}
      {error && <div className="error-box">{error}</div>}

      <div className="lr-filter-row" role="tablist" aria-label="Filter by status">
        {FILTERS.map((f) => (
          <button
            key={f.key}
            role="tab"
            aria-selected={filter === f.key}
            className={`lr-filter${filter === f.key ? ' on' : ''} lr-f-${f.key}`}
            onClick={() => setFilter(f.key)}
          >
            {f.label} <span className="lr-filter-n">{f.count.toLocaleString()}</span>
          </button>
        ))}
      </div>

      {shown.length === 0 ? (
        <EmptyState title="No links here" hint={filter === 'all' ? 'This run has no links.' : 'Nothing with this status.'} />
      ) : (
        <ul className="lr-posts">
          {shown.map((g) => {
            const applied = g.links.filter((l) => statusKey(l) === 'applied').length;
            const o = isOpen(g.post.id);
            return (
              <li key={g.post.id} className={`lr-post${o ? ' open' : ''}`}>
                <div className="lr-post-head">
                  <button type="button" className="lr-post-toggle" onClick={() => toggleOpen(g.post.id)} aria-expanded={o}>
                    <span className="lr-caret" aria-hidden="true" />
                    <span className="lr-post-title">{g.post.title || g.post.url || '(untitled)'}</span>
                  </button>
                  <StatusStrip links={g.links} />
                  <span className="lr-post-count">
                    {applied}/{g.links.length} live
                  </span>
                  {g.post.url && (
                    <a className="btn btn-sm btn-ghost" href={g.post.url} target="_blank" rel="noopener noreferrer">
                      Post ↗
                    </a>
                  )}
                </div>
                {o && (
                  <ul className="lr-links">
                    {g.links.map((l) => (
                      <li key={l.edge_id} className={`lr-link lr-res lr-st-${statusKey(l)}`}>
                        <ItemBadge item={l} />
                        <div className="lr-link-main">
                          <span className="lr-anchor">“{l.anchor}”</span>
                          <span className="lr-to">links to</span>
                          <PostLink url={l.target.url} title={l.target.title} />
                          {(l.error || (statusKey(l) !== 'applied' && l.failure_kind)) && (
                            <div className="lr-reason">{l.error || l.failure_kind}</div>
                          )}
                        </div>
                        <div className="lr-link-meta">
                          {l.verify?.checked_at && <span className="faint">checked {fmtDate(l.verify.checked_at)}</span>}
                        </div>
                        <div className="lr-link-actions">
                          <a
                            className="btn btn-sm lr-open"
                            href={openUrl(l)}
                            target="_blank"
                            rel="noopener"
                            data-hint="Opens the live post in a new tab, scrolled to this link and highlighted."
                          >
                            Open ↗
                          </a>
                        </div>
                      </li>
                    ))}
                  </ul>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {hasMore && (
        <button className="btn" onClick={() => void loadMore()} disabled={loadingMore}>
          {loadingMore ? 'Loading…' : `Load more (${(total - loaded).toLocaleString()} left)`}
        </button>
      )}

      {undo && (
        <Modal
          title="Undo this run?"
          onClose={() => setUndo(false)}
          footer={
            <>
              <button className="btn" onClick={() => setUndo(false)} disabled={busy}>
                Keep links
              </button>
              <button className="btn btn-danger" onClick={() => void doUndo()} disabled={busy}>
                {busy ? 'Undoing…' : `Remove ${(counts.applied ?? 0).toLocaleString()} links`}
              </button>
            </>
          }
        >
          <p>Every link this run applied is removed from the live posts, exactly as it was inserted. Other links stay.</p>
        </Modal>
      )}
    </div>
  );
}

function StatusStrip({ links }: { links: LinkRunItem[] }) {
  const total = links.length || 1;
  const parts: Array<[string, number]> = [];
  for (const k of ['applied', 'not_present', 'failed', 'skipped']) {
    const c = links.filter((l) => statusKey(l) === k).length;
    if (c) parts.push([k, c]);
  }
  return (
    <span className="lr-strip" aria-hidden="true">
      {parts.map(([k, c]) => (
        <span key={k} className={`lr-strip-seg lr-st-${k}`} style={{ width: `${(c / total) * 100}%` }} />
      ))}
    </span>
  );
}

// ---------------------------------------------------------------------------
// 9 Fix
// ---------------------------------------------------------------------------

function StepFix({ run, onRun, onResults }: { run: LinkRun; onRun: (r: LinkRun) => void; onResults: () => void }) {
  const toast = useToast();
  const [refresh, setRefresh] = useState(0);
  const { groups, error } = useRunItems(run.id, `${run.status}-${refresh}`);
  const [repairing, setRepairing] = useState(false);
  const [repairedOnce, setRepairedOnce] = useState(false);
  const afterJob = useAfterJob();

  const all = useMemo(() => groups?.flatMap((g) => g.links) ?? [], [groups]);
  const todo = all.filter(needsFix);
  const skipped = all.filter((l) => l.status === 'skipped');

  const repair = async () => {
    setRepairing(true);
    try {
      const r = await linkRunAction(run.id, 'repair');
      if (r) onRun(r);
      toast.push(`Applying ${todo.length} links properly…`, 'info');
      afterJob(r?.repair_job_id ?? null, () => {
        setRepairing(false);
        setRepairedOnce(true);
        fetchLinkRun(run.id).then(onRun).catch(() => undefined);
        setRefresh((x) => x + 1);
        toast.push('Repair finished', 'success');
      });
    } catch (e) {
      setRepairing(false);
      toast.push(errMsg(e, 'Repair failed'), 'error');
    }
  };

  if (groups === null) return <Skeleton rows={6} />;

  return (
    <StepFrame
      title={todo.length ? 'Apply the missing links properly' : 'Nothing left to fix'}
      lead="For each link SiloForge re-reads the post, finds the paragraph again, tries alternate anchors and, if Contextual is on, lets the AI write a sentence. Anything it still cannot place is skipped with the reason."
      actions={
        <button className="btn" onClick={onResults}>
          Back to results
        </button>
      }
    >
      {error && <div className="error-box">{error}</div>}
      {todo.length > 0 ? (
        <>
          <ul className="lr-fix-list">
            {todo.map((l) => (
              <li key={l.edge_id}>
                <ItemBadge item={l} />
                <span className="lr-fix-text">
                  <span className="lr-anchor">“{l.anchor}”</span> in <b>{l.source.title || l.source.url}</b>
                  <span className="lr-reason">{l.error || l.failure_kind || 'Not found on the live page'}</span>
                </span>
                <a className="btn btn-sm lr-open" href={openUrl(l)} target="_blank" rel="noopener">
                  Open ↗
                </a>
              </li>
            ))}
          </ul>
          <div className="lr-step-foot">
            <div className="spacer" />
            <button className="btn btn-primary lr-cta" onClick={() => void repair()} disabled={repairing}>
              {repairing ? 'Applying…' : `Apply properly to ${todo.length.toLocaleString()} items`}
            </button>
          </div>
        </>
      ) : (
        <EmptyState
          title={repairedOnce ? 'Every link that could be placed is now live' : 'Every link is on the live page'}
          hint="Use Verify again on Results to re-check later."
        />
      )}
      {skipped.length > 0 && (
        <>
          <div className="lr-field-label">Skipped, with the reason</div>
          <ul className="lr-fix-list">
            {skipped.map((l) => (
              <li key={l.edge_id}>
                <ItemBadge item={l} />
                <span className="lr-fix-text">
                  <span className="lr-anchor">“{l.anchor}”</span> in <b>{l.source.title || l.source.url}</b>
                  <span className="lr-reason">{l.error || l.failure_kind || 'No reason given'}</span>
                </span>
                <a className="btn btn-sm lr-open" href={l.live_url || l.source.url} target="_blank" rel="noopener">
                  Post ↗
                </a>
              </li>
            ))}
          </ul>
        </>
      )}
    </StepFrame>
  );
}

