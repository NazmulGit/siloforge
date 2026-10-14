import { useCallback, useEffect, useState } from 'react';
import {
  createIgnoreKeyword,
  createKeywordRule,
  deleteIgnoreKeyword,
  deleteKeywordRule,
  fetchIgnoreKeywords,
  fetchKeywordRules,
  fetchLinksReport,
  patchKeywordRule,
  runKeywordLinker,
} from '../api';
import { useJobTracker } from '../hooks';
import { useToast } from '../toast';
import {
  ANCHOR_TYPES,
  ANCHOR_TYPE_HINTS,
  type AnchorType,
  type IgnoreKeyword,
  type KeywordRule,
  type LinkReportRow,
} from '../types';
import { EmptyState } from './Skeleton';

export function KeywordLinkerPanel({ siteId }: { siteId: string }) {
  const toast = useToast();
  const track = useJobTracker();
  const [rules, setRules] = useState<KeywordRule[]>([]);
  const [posts, setPosts] = useState<LinkReportRow[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [keyword, setKeyword] = useState('');
  const [targetUrl, setTargetUrl] = useState('');
  const [anchorType, setAnchorType] = useState<AnchorType>('exact');
  const [maxLinks, setMaxLinks] = useState(3);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [r, p] = await Promise.all([fetchKeywordRules(siteId), fetchLinksReport(siteId)]);
      setRules(r);
      setPosts(p);
      if (!targetUrl && p.length > 0) setTargetUrl(p[0].url);
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Failed to load rules', 'error');
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [siteId]);

  useEffect(() => {
    void load();
  }, [load]);

  async function add() {
    if (!keyword.trim() || !targetUrl.trim() || busy) return;
    setBusy(true);
    try {
      await createKeywordRule(siteId, { keyword: keyword.trim(), target_url: targetUrl, anchor_type: anchorType, max_links: maxLinks });
      setKeyword('');
      toast.push('Keyword rule added', 'success');
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not add rule', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function toggle(rule: KeywordRule) {
    try {
      await patchKeywordRule(rule.id, { active: !rule.active });
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Update failed', 'error');
    }
  }

  async function remove(rule: KeywordRule) {
    try {
      await deleteKeywordRule(rule.id);
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Delete failed', 'error');
    }
  }

  async function run() {
    if (busy) return;
    setBusy(true);
    try {
      const { job_run_id } = await runKeywordLinker(siteId);
      toast.push('Running keyword linker…', 'info');
      track(siteId, job_run_id, 'Keyword linker', () => void load());
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not start', 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div>
      <div className="card" style={{ marginBottom: 14 }}>
        <div className="card-title">Add a keyword rule</div>
        <div className="page-sub" style={{ marginBottom: 10 }}>
          Links are placed wherever the keyword appears in your content, pointing at the target page.
        </div>
        <div className="rule-form">
          <input className="input" placeholder="keyword (e.g. espresso machine)" value={keyword} onChange={(e) => setKeyword(e.target.value)} />
          <select className="input" value={targetUrl} onChange={(e) => setTargetUrl(e.target.value)}>
            {posts.length === 0 && <option value="">no posts yet</option>}
            {posts.map((p) => (
              <option key={p.post_id} value={p.url}>
                {p.title}
              </option>
            ))}
          </select>
          <select
            className="input"
            value={anchorType}
            onChange={(e) => setAnchorType(e.target.value as AnchorType)}
            aria-label="Anchor type"
            data-hint={`How the anchor text is written. ${anchorType} — ${ANCHOR_TYPE_HINTS[anchorType]}.`}
          >
            {ANCHOR_TYPES.map((t) => (
              <option key={t} value={t} title={ANCHOR_TYPE_HINTS[t]}>
                {t}
              </option>
            ))}
          </select>
          <input className="input" type="number" min={1} max={100} value={maxLinks} onChange={(e) => setMaxLinks(Number(e.target.value))} title="max links" style={{ width: 90 }} />
          <button
            className="btn btn-primary"
            disabled={busy || !keyword.trim() || !targetUrl}
            onClick={() => void add()}
            data-hint="Saves this keyword→target rule. It won't place any links until you press Run Keyword Linker."
          >
            Add rule
          </button>
        </div>
      </div>

      <div className="table-toolbar">
        <span className="muted">{loading ? 'Loading…' : `${rules.length} rule${rules.length === 1 ? '' : 's'}`}</span>
        <div className="spacer" />
        <button
          className="btn btn-primary"
          disabled={busy || rules.length === 0}
          onClick={() => void run()}
          data-hint="Scans your content and generates link suggestions for every active keyword rule. Creates suggestions to review — it doesn't apply links directly."
        >
          Run Keyword Linker
        </button>
      </div>

      {!loading && rules.length === 0 ? (
        <EmptyState title="No keyword rules yet" hint="Add a rule above, then Run Keyword Linker to generate suggestions." />
      ) : (
        <table className="tbl">
          <thead>
            <tr>
              <th>KEYWORD</th>
              <th>TARGET</th>
              <th>TYPE</th>
              <th>MAX</th>
              <th>ACTIVE</th>
              <th>ACTIONS</th>
            </tr>
          </thead>
          <tbody>
            {rules.map((r) => (
              <tr key={r.id}>
                <td>“{r.keyword}”</td>
                <td className="mono-sm">{r.target_url}</td>
                <td>
                  <span className={`chip chip-type-${r.anchor_type}`} title={ANCHOR_TYPE_HINTS[r.anchor_type]}>
                    {r.anchor_type}
                  </span>
                </td>
                <td>{r.max_links}</td>
                <td>
                  <button
                    className={`btn btn-sm ${r.active ? 'btn-primary' : ''}`}
                    onClick={() => void toggle(r)}
                    data-hint="Enable or disable this rule. Only active rules are used when you run the keyword linker."
                  >
                    {r.active ? 'on' : 'off'}
                  </button>
                </td>
                <td>
                  <button
                    className="btn btn-sm btn-danger"
                    onClick={() => void remove(r)}
                    data-hint="Permanently delete this keyword rule. Already-applied links are not removed."
                  >
                    Delete
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

export function IgnoreKeywordsPanel({ siteId }: { siteId: string }) {
  const toast = useToast();
  const [items, setItems] = useState<IgnoreKeyword[]>([]);
  const [loading, setLoading] = useState(true);
  const [phrase, setPhrase] = useState('');
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setItems(await fetchIgnoreKeywords(siteId));
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Failed to load', 'error');
    } finally {
      setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [siteId]);

  useEffect(() => {
    void load();
  }, [load]);

  async function add() {
    if (!phrase.trim() || busy) return;
    setBusy(true);
    try {
      await createIgnoreKeyword(siteId, phrase.trim());
      setPhrase('');
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not add', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function remove(item: IgnoreKeyword) {
    try {
      await deleteIgnoreKeyword(item.id);
      await load();
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Delete failed', 'error');
    }
  }

  return (
    <div>
      <div className="card" style={{ marginBottom: 14 }}>
        <div className="card-title">Ignore keywords</div>
        <div className="page-sub" style={{ marginBottom: 10 }}>
          Phrases the linker must never turn into anchor text (brand safety / stop-list).
        </div>
        <div className="rule-form">
          <input
            className="input"
            placeholder="phrase to ignore"
            value={phrase}
            onChange={(e) => setPhrase(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') void add();
            }}
          />
          <button
            className="btn btn-primary"
            disabled={busy || !phrase.trim()}
            onClick={() => void add()}
            data-hint="Adds this phrase to the stop-list so the linker will never use it as anchor text."
          >
            Add
          </button>
        </div>
      </div>

      {!loading && items.length === 0 ? (
        <EmptyState title="No ignored phrases" hint="Add phrases above to keep them out of anchors." />
      ) : (
        <div className="chip-list">
          {items.map((it) => (
            <span className="chip removable" key={it.id}>
              {it.phrase}
              <button
                className="chip-x"
                onClick={() => void remove(it)}
                aria-label="remove"
                data-hint="Remove this phrase from the ignore list so the linker may use it as anchor text again."
              >
                ×
              </button>
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
