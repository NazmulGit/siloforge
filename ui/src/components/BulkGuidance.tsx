import { useState } from 'react';
import type { InsertModule } from '../types';

const SAFETY_KEY = 'sf_bulk_safety_dismissed';

const MODULES: Array<{ id: InsertModule; label: string; threshold: number; desc: string }> = [
  {
    id: 'native',
    label: 'Native',
    threshold: 60,
    desc: 'Wraps a link around a phrase already in your content. Nothing is rewritten, nothing is added.',
  },
  {
    id: 'rephrase',
    label: 'Rephrase',
    threshold: 40,
    desc: 'Rewrites one existing sentence in place so the anchor fits naturally. One sentence changes — no new content.',
  },
  {
    id: 'context',
    label: 'Context',
    threshold: 30,
    desc: 'Adds 2–3 new sentences to the most relevant paragraph, carrying the link with them.',
  },
];

/**
 * Collapsible, plain-language explainer of the three insertion modules plus a
 * one-liner on Pipeline vs Single. Bulk tab only. Purely informational.
 */
export function BulkModuleInfo({ mode }: { mode: InsertModule | 'pipeline' }) {
  return (
    <details className="module-info card" open>
      <summary>
        How insertion works
        <span className="module-info-sub">
          {mode === 'pipeline' ? 'Pipeline mode' : `Single mode · ${mode}`}
        </span>
      </summary>
      <div className="module-info-body">
        {MODULES.map((m) => {
          const active = mode === 'pipeline' || mode === m.id;
          return (
            <div className={`module-row${active ? '' : ' dim'}`} key={m.id}>
              <div className="module-head">
                <span className="module-name">{m.label}</span>
                <span className="module-thresh" title="Minimum confidence score to use this module">
                  ≥{m.threshold}
                </span>
              </div>
              <div className="module-desc">{m.desc}</div>
            </div>
          );
        })}
        <div className="module-note">
          <b>Pipeline</b> tries Native → Rephrase → Context and stops at the first that succeeds.
          <b> Single</b> forces exactly one module, so an edge is skipped if that module can't place it.
        </div>
      </div>
    </details>
  );
}

/**
 * Subtle, dismissible best-practice callout shown before "Run Bulk Inject".
 * Dismissal is remembered in localStorage.
 */
export function BulkSafetyNote() {
  const [dismissed, setDismissed] = useState(() => {
    try {
      return localStorage.getItem(SAFETY_KEY) === '1';
    } catch {
      return false;
    }
  });

  if (dismissed) return null;

  function dismiss() {
    try {
      localStorage.setItem(SAFETY_KEY, '1');
    } catch {
      /* private mode / storage disabled — dismiss for this session only */
    }
    setDismissed(true);
  }

  return (
    <div className="info-note" role="note">
      <span className="info-note-icon" aria-hidden="true">
        ℹ
      </span>
      <span className="info-note-text">
        <b>Best practice:</b> back up → test a few posts → review the result → run in bulk.
      </span>
      <button className="info-note-x" onClick={dismiss} aria-label="Dismiss">
        ×
      </button>
    </div>
  );
}
