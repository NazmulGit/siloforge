// Site settings → Linking rules (checklist D2 / D4 + the T8 SOP guardrails).
//
// These are per-site numbers the engine enforces during candidate generation and
// injection, so they are edited here rather than buried in the Bulk tab.
import { useCallback, useEffect, useState } from 'react';
import { fetchSite, patchSiteSettings } from '../api';
import { useToast } from '../toast';
import {
  SILO_TOPOLOGIES,
  TOPOLOGY_HINTS,
  type SiloTopology,
  type SiteSettings,
} from '../types';
import { Skeleton } from './Skeleton';
import { Toggle } from './Toggle';

interface NumField {
  key: 'min_links_per_post' | 'max_links_per_post' | 'max_new_links_per_pass' | 'link_overload_threshold';
  label: string;
  placeholder: string;
  desc: string;
  hint: string;
}

const NUM_FIELDS: NumField[] = [
  {
    key: 'min_links_per_post',
    label: 'Minimum links per post',
    placeholder: '3',
    desc: 'Posts below this are flagged as under-linked.',
    hint: 'The floor used to flag under-linked posts. The T8 SOP treats 3 contextual internal links as the minimum for a body post.',
  },
  {
    key: 'max_links_per_post',
    label: 'Maximum links per post',
    placeholder: '10',
    desc: 'Hard ceiling on contextual internal links a post may carry.',
    hint: 'Candidate generation stops adding links to a post once it holds this many. Prevents link-stuffed pages.',
  },
  {
    key: 'max_new_links_per_pass',
    label: 'Max new links per pass',
    placeholder: '5',
    desc: 'How many links a single injection run may add to one post.',
    hint: 'Injecting a large batch into one post in one pass looks unnatural. The T8 SOP lists more than 5 new links in a single pass as a "never do".',
  },
  {
    key: 'link_overload_threshold',
    label: 'Link overload threshold',
    placeholder: '100',
    desc: 'Outbound-link count above which a page is reported as overloaded.',
    hint: 'A reporting threshold, not a hard limit — pages carrying more outbound links than this are counted as link-overloaded in metrics.',
  },
];

type AiKey = 'ai_anchor_refine_max' | 'ai_anchor_refine_concurrency' | 'ai_anchor_refine_timeout_s';

/** Budget for the optional AI anchor refinement (ranges = sf-server settings validator). */
const AI_FIELDS: Array<{ key: AiKey; label: string; def: number; min: number; max: number; desc: string }> = [
  { key: 'ai_anchor_refine_max', label: 'Edges per plan', def: 200, min: 0, max: 5000, desc: 'Top new suggestions (by score) sent to the AI.' },
  { key: 'ai_anchor_refine_concurrency', label: 'Parallel calls', def: 4, min: 1, max: 16, desc: 'AI requests in flight at once.' },
  { key: 'ai_anchor_refine_timeout_s', label: 'Time budget (s)', def: 300, min: 5, max: 3600, desc: 'The plan finishes with heuristic anchors when this runs out.' },
];

function toInput(v: number | undefined): string {
  return v === undefined || v === null ? '' : String(v);
}

function parseNum(raw: string): number | undefined | 'invalid' {
  const t = raw.trim();
  if (t === '') return undefined;
  const n = Number(t);
  if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n)) return 'invalid';
  return n;
}

export function LinkRulesPanel({ siteId }: { siteId: string }) {
  const toast = useToast();
  const [settings, setSettings] = useState<SiteSettings | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});
  const [topology, setTopology] = useState<SiloTopology | ''>('');
  const [skipVerify, setSkipVerify] = useState(false);
  const [aiRefine, setAiRefine] = useState(false);
  const [semanticOff, setSemanticOff] = useState(false);
  const [aiValues, setAiValues] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setLoadError(null);
    try {
      const site = await fetchSite(siteId);
      const s = site.settings ?? {};
      setSettings(s);
      setValues({
        min_links_per_post: toInput(s.min_links_per_post),
        max_links_per_post: toInput(s.max_links_per_post),
        max_new_links_per_pass: toInput(s.max_new_links_per_pass),
        link_overload_threshold: toInput(s.link_overload_threshold),
      });
      setTopology(s.silo_topology ?? '');
      setSkipVerify(s.skip_inject_verify === true);
      setAiRefine(s.ai_anchor_refine === true);
      setSemanticOff(s.anchor_semantic_off === true);
      setAiValues({
        ai_anchor_refine_max: toInput(s.ai_anchor_refine_max),
        ai_anchor_refine_concurrency: toInput(s.ai_anchor_refine_concurrency),
        ai_anchor_refine_timeout_s: toInput(s.ai_anchor_refine_timeout_s),
      });
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : 'Failed to load site settings');
      setSettings(null);
    }
  }, [siteId]);

  useEffect(() => {
    setSettings(null);
    void load();
  }, [load]);

  async function save() {
    if (busy) return;
    const patch: SiteSettings = {};
    for (const f of NUM_FIELDS) {
      const parsed = parseNum(values[f.key] ?? '');
      if (parsed === 'invalid') {
        setError(`${f.label} must be a whole number of 0 or more (leave it blank to use the default).`);
        return;
      }
      if (parsed !== undefined) patch[f.key] = parsed;
    }
    const min = patch.min_links_per_post;
    const max = patch.max_links_per_post;
    if (min !== undefined && max !== undefined && min > max) {
      setError('Minimum links per post cannot be greater than the maximum.');
      return;
    }
    for (const f of AI_FIELDS) {
      const parsed = parseNum(aiValues[f.key] ?? '');
      if (parsed === 'invalid' || (parsed !== undefined && (parsed < f.min || parsed > f.max))) {
        setError(`${f.label} must be a whole number from ${f.min} to ${f.max} (leave it blank for ${f.def}).`);
        return;
      }
      if (parsed !== undefined) patch[f.key] = parsed;
    }
    if (topology) patch.silo_topology = topology;
    patch.skip_inject_verify = skipVerify;
    patch.ai_anchor_refine = aiRefine;
    patch.anchor_semantic_off = semanticOff;
    setError(null);
    setBusy(true);
    try {
      await patchSiteSettings(siteId, patch);
      toast.push('Linking rules saved', 'success');
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Could not save the linking rules');
    } finally {
      setBusy(false);
    }
  }

  if (loadError) return <div className="error-box">{loadError}</div>;
  if (!settings) return <Skeleton rows={6} />;

  return (
    <>
      <div className="card">
        <div className="card-title">Link density</div>
        <p className="muted" style={{ marginTop: 0, fontSize: 13 }}>
          Leave a field blank to use the built-in default. These apply to this site only.
        </p>
        <div className="density-grid">
          {NUM_FIELDS.map((f) => (
            <label key={f.key} className="field">
              {f.label}
              <input
                type="number"
                min={0}
                step={1}
                className="input"
                value={values[f.key] ?? ''}
                placeholder={`default ${f.placeholder}`}
                onChange={(e) => setValues({ ...values, [f.key]: e.target.value })}
                data-hint={f.hint}
              />
              <span className="faint" style={{ fontSize: 12 }}>
                {f.desc}
              </span>
            </label>
          ))}
          <label className="field">
            Default silo topology
            <select
              className="input"
              value={topology}
              onChange={(e) => setTopology(e.target.value as SiloTopology | '')}
              data-hint="The silo shape used when a plan is run without an explicit topology — including scheduled ones."
            >
              <option value="">(none — choose per run)</option>
              {SILO_TOPOLOGIES.map((t) => (
                <option key={t} value={t}>
                  {t} — {TOPOLOGY_HINTS[t]}
                </option>
              ))}
            </select>
            <span className="faint" style={{ fontSize: 12 }}>
              Used by scheduled plans and audits that carry no topology of their own.
            </span>
          </label>
        </div>

        {error && (
          <div className="error-box" style={{ marginTop: 12 }}>
            {error}
          </div>
        )}
      </div>

      <div className="card">
        <div className="card-title">Anchor text</div>
        <div className="settings-list">
          <div className="settings-row">
            <div className="settings-row-text">
              <div className="settings-row-label">AI anchor refinement</div>
              <div className="settings-row-desc">
                After each plan, the AI reviews the anchors of the best new suggestions and may swap in a better
                phrase from the same paragraph. Replies are validated (verbatim, natural, allowed); anything else keeps
                the heuristic anchor. Uses the site&apos;s AI model chain. A single plan can override this
                (&ldquo;AI anchor refinement&rdquo; in the Run plan dialog).
              </div>
            </div>
            <Toggle
              checked={aiRefine}
              onChange={setAiRefine}
              label="AI anchor refinement"
              dataHint="Off by default. Costs one AI call per refined edge, bounded by the budget below. If the AI fails, the plan still completes with heuristic anchors."
            />
          </div>
          <div className="settings-row">
            <div className="settings-row-text">
              <div className="settings-row-label">Semantic phrase scoring</div>
              <div className="settings-row-desc">
                Prefers anchor phrases whose meaning is close to the target post (uses the embeddings). On by
                default.
              </div>
            </div>
            <Toggle
              checked={!semanticOff}
              onChange={(v) => setSemanticOff(!v)}
              label="Semantic phrase scoring"
              dataHint="Turn off only to compare against the plain lexical picker; it adds about a second per thousand edges."
            />
          </div>
        </div>
        {aiRefine && (
          <div className="density-grid" style={{ marginTop: 12 }}>
            {AI_FIELDS.map((f) => (
              <label key={f.key} className="field">
                {f.label}
                <input
                  type="number"
                  min={f.min}
                  max={f.max}
                  step={1}
                  className="input"
                  value={aiValues[f.key] ?? ''}
                  placeholder={`default ${f.def}`}
                  onChange={(e) => setAiValues({ ...aiValues, [f.key]: e.target.value })}
                />
                <span className="faint" style={{ fontSize: 12 }}>
                  {f.desc}
                </span>
              </label>
            ))}
          </div>
        )}
      </div>

      <div className="card">
        <div className="card-title">Injection safety</div>
        <div className="settings-list">
          <div className="settings-row">
            <div className="settings-row-text">
              <div className="settings-row-label">Skip post-injection verify</div>
              <div className="settings-row-desc">
                After writing a link, SiloForge re-fetches the page and confirms the anchor is really
                there. Turning this off is faster — and means a silently failed edit looks like a success.
              </div>
            </div>
            <Toggle
              checked={skipVerify}
              onChange={setSkipVerify}
              label="Skip post-injection verify"
              dataHint="Leave this OFF. The T8 SOP treats the post-injection live verify as mandatory — it is the only check that a link actually landed on the live page."
            />
          </div>
        </div>
        {skipVerify && (
          <div className="warn-note" style={{ marginTop: 12 }}>
            Verification is off. Injections will be reported as applied without confirming the anchor
            exists on the live page — the T8 SOP treats this check as mandatory.
          </div>
        )}
      </div>

      <div className="form-actions">
        <button
          className="btn btn-primary"
          disabled={busy}
          onClick={() => void save()}
          data-hint="Saves these rules to the site. They take effect on the next plan and the next injection run."
        >
          {busy ? 'Saving…' : 'Save linking rules'}
        </button>
      </div>
    </>
  );
}
