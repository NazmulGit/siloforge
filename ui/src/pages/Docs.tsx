import { useEffect, useMemo, useState } from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import { fetchDoc, isUnavailable, v2ErrorMessage } from '../api2';
import { Knowledgebase } from '../components/Knowledgebase';
import { Markdown } from '../components/Markdown';
import { Skeleton } from '../components/Skeleton';
import { sortDocs, useDocs } from '../docs';
import type { DocMeta, DocPage } from '../types2';

/** Pseudo-slug for the built-in operating procedures (the old Settings → Knowledge Base). */
const SOP_SLUG = 'operating-procedures';

export function DocsPage() {
  const { slug: rawSlug } = useParams();
  const navigate = useNavigate();
  const { docs, unavailable, error: listError, loadDocs } = useDocs();
  const [q, setQ] = useState('');
  const [page, setPage] = useState<DocPage | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void loadDocs();
  }, [loadDocs]);

  const sorted = useMemo(() => sortDocs(docs ?? []), [docs]);
  const slug = rawSlug ?? (docs === null ? null : sorted[0]?.slug ?? SOP_SLUG);

  const groups = useMemo(() => {
    const needle = q.trim().toLowerCase();
    const shown = needle ? sorted.filter((d) => `${d.title} ${d.slug} ${d.section ?? ''}`.toLowerCase().includes(needle)) : sorted;
    const map = new Map<string, DocMeta[]>();
    for (const d of shown) {
      const key = d.section || 'Guide';
      map.set(key, [...(map.get(key) ?? []), d]);
    }
    return [...map.entries()];
  }, [sorted, q]);

  useEffect(() => {
    if (!slug || slug === SOP_SLUG) return;
    let alive = true;
    setPage(null);
    setError(null);
    fetchDoc(slug)
      .then((p) => {
        if (alive) setPage(p);
      })
      .catch((e: unknown) => {
        if (!alive) return;
        setError(isUnavailable(e) ? 'This guide page was not found.' : v2ErrorMessage(e, 'Failed to load this page'));
      });
    return () => {
      alive = false;
    };
  }, [slug]);

  const go = (s: string) => navigate(`/docs/${encodeURIComponent(s)}`);

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Docs</h1>
          <div className="page-sub">The SiloForge user guide. Press ? in the top bar on any page to open the matching page here.</div>
        </div>
      </div>
      <div className="docs-layout">
        <nav className="docs-index card" aria-label="Guide pages">
          <input className="input" type="search" placeholder="Filter pages" aria-label="Filter guide pages" value={q} onChange={(e) => setQ(e.target.value)} />
          {docs === null && <Skeleton rows={6} />}
          {unavailable && <div className="muted docs-note">The full guide ships with SiloForge 2.1. This server only has the built-in quick guide.</div>}
          {listError && <div className="error-box">{listError}</div>}
          {groups.map(([section, items]) => (
            <div key={section} className="docs-group">
              <div className="docs-group-head">{section}</div>
              {items.map((d) => (
                <button key={d.slug} type="button" className={`docs-link${d.slug === slug ? ' active' : ''}`} onClick={() => go(d.slug)}>
                  {d.title}
                </button>
              ))}
            </div>
          ))}
          <div className="docs-group">
            <div className="docs-group-head">Built in</div>
            <button type="button" className={`docs-link${slug === SOP_SLUG ? ' active' : ''}`} onClick={() => go(SOP_SLUG)}>
              Operating procedures
            </button>
          </div>
        </nav>
        <article className="docs-body card">
          {slug === SOP_SLUG ? (
            <Knowledgebase />
          ) : error ? (
            <div className="error-box">{error}</div>
          ) : !page ? (
            <Skeleton rows={10} />
          ) : (
            <Markdown source={page.markdown} onDocLink={go} />
          )}
        </article>
      </div>
    </div>
  );
}
