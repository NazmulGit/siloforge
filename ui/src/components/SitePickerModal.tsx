import { useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { fmtDate } from '../format';
import { sitePostCount, type SiteListItem } from '../types';
import { Modal } from './Modal';

/** Search + list of sites. Opened when a site-scoped page is chosen with no site selected. */
export function SitePickerModal({
  sites,
  loading,
  currentId,
  onPick,
  onClose,
}: {
  sites: SiteListItem[];
  loading: boolean;
  currentId: string | null;
  onPick: (id: string) => void;
  onClose: () => void;
}) {
  const [q, setQ] = useState('');
  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    const list = needle ? sites.filter((s) => s.domain.toLowerCase().includes(needle)) : sites;
    return [...list].sort((a, b) => a.domain.localeCompare(b.domain));
  }, [sites, q]);

  return (
    <Modal title="Choose a site" onClose={onClose}>
      {sites.length === 0 && !loading ? (
        <div className="empty">
          <div className="empty-title">No sites yet</div>
          <div className="empty-hint">
            <Link to="/sites" onClick={onClose}>
              Add a site
            </Link>{' '}
            to start crawling and planning links.
          </div>
        </div>
      ) : (
        <>
          <input
            className="input picker-search"
            type="search"
            placeholder="Search sites"
            aria-label="Search sites"
            value={q}
            autoFocus
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && shown[0]) onPick(shown[0].id);
            }}
          />
          <ul className="picker-list" role="listbox" aria-label="Sites">
            {loading && sites.length === 0 && <li className="muted picker-empty">Loading sites…</li>}
            {!loading && shown.length === 0 && <li className="muted picker-empty">No site matches “{q}”.</li>}
            {shown.map((s) => (
              <li key={s.id}>
                <button
                  type="button"
                  role="option"
                  aria-selected={s.id === currentId}
                  className={`picker-item${s.id === currentId ? ' current' : ''}`}
                  onClick={() => onPick(s.id)}
                >
                  <span className="picker-domain">{s.domain}</span>
                  <span className="picker-meta">
                    {sitePostCount(s).toLocaleString()} posts
                    {s.last_sync_at ? `, synced ${fmtDate(s.last_sync_at)}` : ', never synced'}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </Modal>
  );
}
