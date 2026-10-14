import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { fetchDoc, isUnavailable, v2ErrorMessage } from '../api2';
import { sortDocs, useDocs } from '../docs';
import type { DocPage } from '../types2';
import { Icon } from './Icon';
import { Markdown } from './Markdown';
import { Skeleton } from './Skeleton';

/** Right-side reader for one guide page. slug=null while the page→doc lookup runs. */
export function DocDrawer({ slug, onNavigate, onClose }: { slug: string | null; onNavigate: (slug: string) => void; onClose: () => void }) {
  const { docs, unavailable, loadDocs } = useDocs();
  const [page, setPage] = useState<DocPage | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [missing, setMissing] = useState(false);

  useEffect(() => {
    void loadDocs();
  }, [loadDocs]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  useEffect(() => {
    if (!slug) return;
    let alive = true;
    setPage(null);
    setError(null);
    setMissing(false);
    fetchDoc(slug)
      .then((p) => {
        if (alive) setPage(p);
      })
      .catch((e: unknown) => {
        if (!alive) return;
        if (isUnavailable(e)) setMissing(true);
        else setError(v2ErrorMessage(e, 'Failed to load this page'));
      });
    return () => {
      alive = false;
    };
  }, [slug]);

  const list = sortDocs(docs ?? []);
  const noDocs = unavailable || (docs !== null && docs.length === 0);

  return (
    <div className="drawer-overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <aside className="drawer doc-drawer" role="dialog" aria-modal="true" aria-label="Help">
        <div className="drawer-head">
          <div className="drawer-title">{page?.title ?? 'Help'}</div>
          {list.length > 0 && (
            <select className="doc-jump" aria-label="Jump to guide page" value={slug ?? ''} onChange={(e) => e.target.value && onNavigate(e.target.value)}>
              <option value="" disabled>
                Guide pages…
              </option>
              {list.map((d) => (
                <option key={d.slug} value={d.slug}>
                  {d.section ? `${d.section}: ` : ''}
                  {d.title}
                </option>
              ))}
            </select>
          )}
          <button className="modal-close" onClick={onClose} aria-label="Close help">
            <Icon name="close" />
          </button>
        </div>
        <div className="drawer-body">
          {noDocs ? (
            <div className="empty">
              <div className="empty-title">The user guide is not available on this server</div>
              <div className="empty-hint">
                It ships with SiloForge 2.1. The quick guides on the <Link to="/docs" onClick={onClose}>Docs page</Link> still work.
              </div>
            </div>
          ) : error ? (
            <div className="error-box">{error}</div>
          ) : missing ? (
            <div className="muted">This guide page was not found. Pick another one from the list above.</div>
          ) : !page ? (
            <Skeleton rows={6} />
          ) : (
            <Markdown source={page.markdown} onDocLink={onNavigate} />
          )}
        </div>
        {page && (
          <div className="drawer-foot">
            <Link to={`/docs/${encodeURIComponent(page.slug)}`} onClick={onClose}>
              Open in the Docs page
            </Link>
          </div>
        )}
      </aside>
    </div>
  );
}
