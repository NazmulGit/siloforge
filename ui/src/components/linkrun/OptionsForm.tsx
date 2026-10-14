// Shared Link Run options (spec §2): strategies + filters, linking mode cards with
// diagrams, crawl choice and switches. Used by the Link Run stepper (steps 1, 3, 4)
// and by the Auto-Linker card in site settings (all sections at once).
import { useEffect, useId, useState, type ReactElement } from 'react';
import { fetchClusters } from '../../api';
import type { ClusterInfo } from '../../types';
import type { LinkRunCrawl, LinkRunFilters, LinkRunMode, LinkRunOptions, LinkStrategy } from '../../types2';
import { Toggle } from '../Toggle';
import './linkrun.css';

export const DEFAULT_FILTERS: LinkRunFilters = {
  min_score: 0.55,
  max_links_per_post: 5,
  max_new_per_post: 3,
  max_total: 500,
  anchor_types: ['exact', 'partial', 'lsi', 'branded'],
  max_exact_share: 0.35,
  clusters: [],
  include_post_ids: [],
  exclude_post_ids: [],
  exclude_url_patterns: [],
  post_types: ['post', 'page'],
};

export const DEFAULT_OPTIONS: LinkRunOptions = {
  crawl: 'if_stale',
  strategies: {
    in_text: true,
    contextual: false,
    semantic: true,
    keyword_rules: true,
    silo: true,
    orphan_rescue: true,
    money_pages: false,
    fix_broken: false,
  },
  mode: 'opportunity',
  filters: DEFAULT_FILTERS,
  ai_refine: false,
  verify: true,
  approve: 'selected',
};

/** Fill a partial options object (from the server or settings) with defaults. */
export function normalizeOptions(p: unknown): LinkRunOptions {
  const o = (p && typeof p === 'object' ? p : {}) as Partial<LinkRunOptions>;
  return {
    ...DEFAULT_OPTIONS,
    ...o,
    strategies: { ...DEFAULT_OPTIONS.strategies, ...(o.strategies ?? {}) },
    filters: { ...DEFAULT_FILTERS, ...(o.filters ?? {}) },
    approve: 'selected',
  };
}

export const STRATEGY_DEFS: Array<{ key: LinkStrategy; label: string; line: string }> = [
  { key: 'in_text', label: 'In-text', line: 'Link a phrase that is already in the paragraph. The text is not changed.' },
  { key: 'contextual', label: 'Contextual', line: 'AI writes or rewrites one sentence that carries the link. Needs an AI chain.' },
  { key: 'semantic', label: 'Semantic', line: 'Find targets by meaning: paragraph-to-post embedding similarity.' },
  { key: 'keyword_rules', label: 'Keyword rules', line: 'Apply this site’s keyword → URL rules.' },
  { key: 'silo', label: 'Silo', line: 'Add the links the chosen silo shape needs.' },
  { key: 'orphan_rescue', label: 'Orphan rescue', line: 'Put posts with no or few inbound links first.' },
  { key: 'money_pages', label: 'Money pages', line: 'Boost targets listed as money pages in Linking Rules.' },
  { key: 'fix_broken', label: 'Fix broken links', line: 'Swap links to broken internal URLs for the best live target.' },
];

export const STRATEGY_LABEL: Record<string, string> = Object.fromEntries(STRATEGY_DEFS.map((s) => [s.key, s.label]));

export const MODE_DEFS: Array<{ key: LinkRunMode; label: string; line: string }> = [
  { key: 'opportunity', label: 'Link opportunity', line: 'The best-scoring links anywhere on the site, no silo shape.' },
  { key: 'reverse', label: 'Reverse silo', line: 'Support posts link up to the pillar, so equity flows to the money page.' },
  { key: 'serial', label: 'Serial', line: 'A chain: each post in a cluster links to the next one.' },
  { key: 'priority', label: 'Priority', line: 'Concentrate links on your highest-priority pages.' },
  { key: 'circle', label: 'Circle', line: 'A loop: every post links onward and the last links back to the first.' },
  { key: 'bidirectional', label: 'Bidirectional', line: 'The pillar and every spoke link to each other.' },
];

export const MODE_LABEL: Record<string, string> = Object.fromEntries(MODE_DEFS.map((m) => [m.key, m.label]));

const CRAWL_DEFS: Array<{ key: LinkRunCrawl; label: string; line: string }> = [
  { key: 'if_stale', label: 'Re-crawl if stale', line: 'Crawl first when the last crawl is older than 24 hours.' },
  { key: 'always', label: 'Always crawl', line: 'Fetch every post again before planning.' },
  { key: 'skip', label: 'Use last crawl', line: 'Plan from the content already indexed.' },
];

const ANCHOR_CHOICES = ['exact', 'partial', 'lsi', 'branded', 'generic', 'naked', 'cta'];

// ---------------------------------------------------------------------------
// Mode diagrams (inline SVG, currentColor + tokens)
// ---------------------------------------------------------------------------

type Pt = [number, number];

export function ModeDiagram({ mode }: { mode: LinkRunMode }) {
  const uid = useId().replace(/[^a-zA-Z0-9]/g, '');
  const arrow = `lr-arrow-${uid}`;
  const arrow2 = `lr-arrow2-${uid}`;
  const P: Pt = [60, 14];
  const line = (a: Pt, b: Pt, opts: { both?: boolean; w?: number; dashed?: boolean } = {}) => {
    // shorten to node edge (r ≈ 6)
    const dx = b[0] - a[0];
    const dy = b[1] - a[1];
    const len = Math.hypot(dx, dy) || 1;
    const r = 7.5;
    const x1 = a[0] + (dx / len) * r;
    const y1 = a[1] + (dy / len) * r;
    const x2 = b[0] - (dx / len) * r;
    const y2 = b[1] - (dy / len) * r;
    return (
      <line
        key={`${a.join()}-${b.join()}`}
        x1={x1}
        y1={y1}
        x2={x2}
        y2={y2}
        strokeWidth={opts.w ?? 1.4}
        strokeDasharray={opts.dashed ? '2.5 2.5' : undefined}
        markerEnd={`url(#${arrow})`}
        markerStart={opts.both ? `url(#${arrow2})` : undefined}
      />
    );
  };
  const node = (p: Pt, pillar = false, k = '') => (
    <circle key={`n${k}${p.join()}`} cx={p[0]} cy={p[1]} r={pillar ? 7 : 5} className={pillar ? 'lr-node-pillar' : 'lr-node'} />
  );

  let edges: ReactElement[] = [];
  let nodes: ReactElement[] = [];
  switch (mode) {
    case 'opportunity': {
      const pts: Pt[] = [
        [16, 18],
        [44, 40],
        [64, 12],
        [92, 30],
        [104, 50],
        [28, 50],
      ];
      edges = [line(pts[0], pts[2]), line(pts[5], pts[1]), line(pts[1], pts[3]), line(pts[4], pts[3], { dashed: true })];
      nodes = pts.map((p) => node(p));
      break;
    }
    case 'reverse': {
      const s: Pt[] = [
        [22, 48],
        [60, 50],
        [98, 48],
      ];
      edges = s.map((p) => line(p, P));
      nodes = [node(P, true), ...s.map((p) => node(p))];
      break;
    }
    case 'serial': {
      const s: Pt[] = [
        [14, 32],
        [45, 32],
        [76, 32],
        [106, 32],
      ];
      edges = [line(s[0], s[1]), line(s[1], s[2]), line(s[2], s[3])];
      nodes = s.map((p) => node(p));
      break;
    }
    case 'priority': {
      const s: Pt[] = [
        [18, 46],
        [48, 52],
        [80, 52],
        [104, 40],
      ];
      edges = [line(s[0], P, { w: 2.4 }), line(s[1], P, { w: 2 }), line(s[2], P, { w: 1.4 }), line(s[3], s[2], { w: 1, dashed: true })];
      nodes = [node(P, true), ...s.map((p) => node(p))];
      break;
    }
    case 'circle': {
      const c: Pt = [60, 32];
      const s: Pt[] = [0, 1, 2, 3, 4].map((i) => {
        const a = -Math.PI / 2 + (i * 2 * Math.PI) / 5;
        return [c[0] + Math.cos(a) * 34, c[1] + Math.sin(a) * 21] as Pt;
      });
      edges = s.map((p, i) => line(p, s[(i + 1) % s.length]));
      nodes = s.map((p) => node(p));
      break;
    }
    case 'bidirectional': {
      const c: Pt = [60, 32];
      const s: Pt[] = [
        [18, 14],
        [102, 14],
        [18, 50],
        [102, 50],
      ];
      edges = s.map((p) => line(c, p, { both: true }));
      nodes = [node(c, true), ...s.map((p) => node(p))];
      break;
    }
  }
  return (
    <svg className="lr-diagram" viewBox="0 0 120 64" width="120" height="64" aria-hidden="true" focusable="false">
      <defs>
        <marker id={arrow} viewBox="0 0 8 8" refX="7" refY="4" markerWidth="5" markerHeight="5" orient="auto">
          <path d="M0 0 L8 4 L0 8 z" className="lr-arrowhead" />
        </marker>
        <marker id={arrow2} viewBox="0 0 8 8" refX="1" refY="4" markerWidth="5" markerHeight="5" orient="auto">
          <path d="M8 0 L0 4 L8 8 z" className="lr-arrowhead" />
        </marker>
      </defs>
      <g className="lr-edges">{edges}</g>
      <g>{nodes}</g>
    </svg>
  );
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

export function CrawlChoice({ value, onChange }: { value: LinkRunCrawl; onChange: (v: LinkRunCrawl) => void }) {
  return (
    <div className="lr-radio-row" role="radiogroup" aria-label="Crawl before planning">
      {CRAWL_DEFS.map((c) => (
        <label key={c.key} className={`lr-radio${value === c.key ? ' on' : ''}`}>
          <input type="radio" name="lr-crawl" checked={value === c.key} onChange={() => onChange(c.key)} />
          <span>
            <b>{c.label}</b>
            <span className="lr-line">{c.line}</span>
          </span>
        </label>
      ))}
    </div>
  );
}

export function ModePicker({ value, onChange }: { value: LinkRunMode; onChange: (v: LinkRunMode) => void }) {
  return (
    <div className="lr-modes" role="radiogroup" aria-label="Linking mode">
      {MODE_DEFS.map((m) => (
        <button
          key={m.key}
          type="button"
          role="radio"
          aria-checked={value === m.key}
          className={`lr-mode${value === m.key ? ' on' : ''}`}
          onClick={() => onChange(m.key)}
        >
          <ModeDiagram mode={m.key} />
          <span className="lr-mode-label">{m.label}</span>
          <span className="lr-line">{m.line}</span>
        </button>
      ))}
    </div>
  );
}

function listText(v: string[]): string {
  return v.join(', ');
}
function parseList(s: string): string[] {
  return s
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter(Boolean);
}

function NumField({
  label,
  value,
  min,
  max,
  onChange,
  hint,
}: {
  label: string;
  value: number;
  min: number;
  max: number;
  onChange: (v: number) => void;
  hint: string;
}) {
  return (
    <label className="field lr-num" data-hint={hint}>
      {label}
      <input
        type="number"
        className="input"
        min={min}
        max={max}
        value={Number.isFinite(value) ? value : ''}
        onChange={(e) => {
          const n = Number(e.target.value);
          if (Number.isFinite(n)) onChange(Math.max(min, Math.min(max, Math.round(n))));
        }}
      />
    </label>
  );
}

export function StrategiesForm({
  value,
  onChange,
  siteId,
  counts,
}: {
  value: LinkRunOptions;
  onChange: (v: LinkRunOptions) => void;
  siteId?: string | null;
  /** Opportunity counts by strategy, shown next to each checkbox when known. */
  counts?: Record<string, number>;
}) {
  const f = value.filters;
  const setF = (patch: Partial<LinkRunFilters>) => onChange({ ...value, filters: { ...f, ...patch } });
  const [clusters, setClusters] = useState<ClusterInfo[] | null>(null);
  const noneTicked = !Object.values(value.strategies).some(Boolean);

  useEffect(() => {
    if (!siteId) return;
    let live = true;
    fetchClusters(siteId)
      .then((c) => live && setClusters(c))
      .catch(() => live && setClusters([]));
    return () => {
      live = false;
    };
  }, [siteId]);

  const toggleIn = (arr: string[], v: string) => (arr.includes(v) ? arr.filter((x) => x !== v) : [...arr, v]);
  const extraTypes = f.post_types.filter((t) => t !== 'post' && t !== 'page');

  return (
    <div className="lr-strategies">
      <div className="lr-checks">
        {STRATEGY_DEFS.map((s) => {
          const n = counts?.[s.key];
          return (
            <label key={s.key} className={`lr-check${value.strategies[s.key] ? ' on' : ''}`}>
              <input
                type="checkbox"
                checked={value.strategies[s.key]}
                onChange={(e) => onChange({ ...value, strategies: { ...value.strategies, [s.key]: e.target.checked } })}
              />
              <span>
                <b>
                  {s.label}
                  {typeof n === 'number' && <span className="lr-check-count">{n.toLocaleString()}</span>}
                </b>
                <span className="lr-line">{s.line}</span>
              </span>
            </label>
          );
        })}
      </div>
      {noneTicked && <div className="error-box">Tick at least one strategy.</div>}

      <div className="lr-filters">
        <div className="lr-filters-title">Filters</div>
        <div className="lr-slider" data-hint="Suggestions scoring below this are dropped. Higher means fewer, more relevant links.">
          <span>Minimum score</span>
          <input
            type="range"
            min={0}
            max={1}
            step={0.01}
            value={f.min_score}
            onChange={(e) => setF({ min_score: Number(e.target.value) })}
            aria-label="Minimum score"
          />
          <output className="mono">{f.min_score.toFixed(2)}</output>
        </div>
        <div className="lr-num-row">
          <NumField
            label="Max links per post"
            value={f.max_links_per_post}
            min={1}
            max={100}
            onChange={(v) => setF({ max_links_per_post: v })}
            hint="A post is never pushed above this many contextual internal links."
          />
          <NumField
            label="Max new per post"
            value={f.max_new_per_post}
            min={1}
            max={20}
            onChange={(v) => setF({ max_new_per_post: v })}
            hint="How many links this run may add to a single post. More than 5 in one pass looks unnatural."
          />
          <NumField
            label="Max total links"
            value={f.max_total}
            min={1}
            max={100000}
            onChange={(v) => setF({ max_total: v })}
            hint="Upper bound for the whole run."
          />
        </div>
        <div className="lr-field-label">Anchor types</div>
        <div className="lr-chips">
          {ANCHOR_CHOICES.map((t) => (
            <label key={t} className={`lr-chip${f.anchor_types.includes(t) ? ' on' : ''}`}>
              <input
                type="checkbox"
                checked={f.anchor_types.includes(t)}
                onChange={() => setF({ anchor_types: toggleIn(f.anchor_types, t) })}
              />
              {t}
            </label>
          ))}
        </div>
        <div className="lr-slider" data-hint="Cap on the share of exact-match anchors across the run, to keep the profile natural.">
          <span>Exact-match share</span>
          <input
            type="range"
            min={0}
            max={1}
            step={0.05}
            value={f.max_exact_share}
            onChange={(e) => setF({ max_exact_share: Number(e.target.value) })}
            aria-label="Maximum exact-match share"
          />
          <output className="mono">{Math.round(f.max_exact_share * 100)}%</output>
        </div>

        <details className="lr-more">
          <summary>Scope: clusters, post types and exclusions</summary>
          <div className="lr-more-body">
            <div className="lr-field-label">Clusters {f.clusters.length === 0 && <span className="faint">(all)</span>}</div>
            {clusters === null ? (
              <div className="faint">{siteId ? 'Loading clusters…' : 'Pick a site to list clusters.'}</div>
            ) : clusters.length === 0 ? (
              <div className="faint">No clusters yet. Run a plan to build them.</div>
            ) : (
              <div className="lr-chips lr-chips-scroll">
                {clusters.map((c) => (
                  <label key={c.id} className={`lr-chip${f.clusters.includes(c.id) ? ' on' : ''}`} title={c.pillar_url ?? undefined}>
                    <input type="checkbox" checked={f.clusters.includes(c.id)} onChange={() => setF({ clusters: toggleIn(f.clusters, c.id) })} />
                    {c.label} <span className="faint">{c.members}</span>
                  </label>
                ))}
              </div>
            )}
            <div className="lr-field-label">Post types</div>
            <div className="lr-chips">
              {['post', 'page'].map((t) => (
                <label key={t} className={`lr-chip${f.post_types.includes(t) ? ' on' : ''}`}>
                  <input type="checkbox" checked={f.post_types.includes(t)} onChange={() => setF({ post_types: toggleIn(f.post_types, t) })} />
                  {t}
                </label>
              ))}
              <input
                className="input lr-inline-input"
                placeholder="other types, comma-separated"
                defaultValue={listText(extraTypes)}
                onBlur={(e) =>
                  setF({ post_types: [...f.post_types.filter((t) => t === 'post' || t === 'page'), ...parseList(e.target.value)] })
                }
                aria-label="Other post types"
              />
            </div>
            <div className="lr-num-row">
              <label className="field">
                Only these post IDs
                <input
                  className="input"
                  placeholder="empty = all posts"
                  defaultValue={listText(f.include_post_ids)}
                  onBlur={(e) => setF({ include_post_ids: parseList(e.target.value) })}
                />
              </label>
              <label className="field">
                Skip these post IDs
                <input
                  className="input"
                  placeholder="e.g. 12, 87"
                  defaultValue={listText(f.exclude_post_ids)}
                  onBlur={(e) => setF({ exclude_post_ids: parseList(e.target.value) })}
                />
              </label>
            </div>
            <label className="field">
              Skip URLs matching (one pattern per line)
              <textarea
                className="input"
                rows={3}
                placeholder={'/tag/\n/author/'}
                defaultValue={f.exclude_url_patterns.join('\n')}
                onBlur={(e) =>
                  setF({
                    exclude_url_patterns: e.target.value
                      .split('\n')
                      .map((x) => x.trim())
                      .filter(Boolean),
                  })
                }
              />
            </label>
          </div>
        </details>
      </div>
    </div>
  );
}

export function RunSwitches({ value, onChange }: { value: LinkRunOptions; onChange: (v: LinkRunOptions) => void }) {
  return (
    <div className="settings-list">
      <div className="settings-row">
        <div className="settings-row-text">
          <div className="settings-row-label">Refine anchors with AI</div>
          <div className="settings-row-desc">The AI polishes the top anchors. Replies are validated; failures keep the original anchor.</div>
        </div>
        <Toggle checked={value.ai_refine} onChange={(v) => onChange({ ...value, ai_refine: v })} label="Refine anchors with AI" />
      </div>
      <div className="settings-row">
        <div className="settings-row-text">
          <div className="settings-row-label">High priority (stronger AI model)</div>
          <div className="settings-row-desc">
            Every AI task of this run uses the high tier. Costs more tokens; use it for money pages and important launches.
          </div>
        </div>
        <Toggle
          checked={!!value.high_priority}
          onChange={(v) => {
            const { high_priority: _drop, ...rest } = value;
            void _drop;
            onChange(v ? { ...rest, high_priority: true } : rest);
          }}
          label="High priority"
        />
      </div>
      <div className="settings-row">
        <div className="settings-row-text">
          <div className="settings-row-label">Check every link on the live page</div>
          <div className="settings-row-desc">After deploy, fetch each post and confirm the link is really there.</div>
        </div>
        <Toggle checked={value.verify} onChange={(v) => onChange({ ...value, verify: v })} label="Verify after deploy" />
      </div>
    </div>
  );
}

/** Every option on one form (Auto-Linker). The stepper uses the pieces above step by step. */
export function OptionsForm({
  value,
  onChange,
  siteId,
}: {
  value: LinkRunOptions;
  onChange: (v: LinkRunOptions) => void;
  siteId?: string | null;
}) {
  return (
    <div className="lr-options">
      <div className="lr-field-label">Crawl</div>
      <CrawlChoice value={value.crawl} onChange={(c) => onChange({ ...value, crawl: c })} />
      <div className="lr-field-label">Strategies</div>
      <StrategiesForm value={value} onChange={onChange} siteId={siteId} />
      <div className="lr-field-label">Linking mode</div>
      <ModePicker value={value.mode} onChange={(m) => onChange({ ...value, mode: m })} />
      <RunSwitches value={value} onChange={onChange} />
    </div>
  );
}
