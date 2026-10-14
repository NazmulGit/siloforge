import { useCallback, useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { fetchNotifications, isUnavailable, markNotificationsRead } from '../api2';
import { fmtDate } from '../format';
import type { NotificationRow } from '../types2';

const POLL_MS = 30000;

/**
 * Top-bar bell: unread count from GET /api/v2/notifications?unread=1 (polled every
 * 30 s), dropdown with the latest notifications, mark-one / mark-all read.
 * Hides itself entirely when the server has no v2 notifications.
 */
export function NotificationBell() {
  const [unread, setUnread] = useState(0);
  const [available, setAvailable] = useState(true);
  const [open, setOpen] = useState(false);
  const [items, setItems] = useState<NotificationRow[] | null>(null);
  const alive = useRef(true);

  const refreshCount = useCallback(async () => {
    try {
      const res = await fetchNotifications({ unread: true, limit: 50 });
      if (!alive.current) return;
      setUnread(res.unread ?? res.notifications.filter((n) => !n.read_at).length);
    } catch (e) {
      if (alive.current && isUnavailable(e)) setAvailable(false);
    }
  }, []);

  useEffect(() => {
    alive.current = true;
    let timer: number | undefined;
    const tick = async () => {
      await refreshCount();
      if (alive.current) timer = window.setTimeout(() => void tick(), POLL_MS);
    };
    void tick();
    return () => {
      alive.current = false;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [refreshCount]);

  const loadList = useCallback(async () => {
    try {
      const res = await fetchNotifications({ limit: 20 });
      if (alive.current) setItems(res.notifications);
    } catch {
      if (alive.current) setItems([]);
    }
  }, []);

  async function markRead(body: { ids?: number[]; all?: boolean }) {
    try {
      await markNotificationsRead(body);
    } catch {
      /* best effort */
    }
    await Promise.all([refreshCount(), loadList()]);
  }

  if (!available) return null;

  return (
    <div className="bell-wrap">
      <button
        type="button"
        className={`bell-btn${unread > 0 ? ' has-unread' : ''}`}
        aria-label={`Notifications${unread ? ` (${unread} unread)` : ''}`}
        data-hint="Notifications: finished and failed jobs, backups and system alerts."
        onClick={() => {
          const next = !open;
          setOpen(next);
          if (next) void loadList();
        }}
      >
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
          <path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9" />
          <path d="M13.73 21a2 2 0 0 1-3.46 0" />
        </svg>
        {unread > 0 && <span className="bell-count">{unread > 99 ? '99+' : unread}</span>}
      </button>
      {open && (
        <>
          <div className="popover-backdrop" onClick={() => setOpen(false)} />
          <div className="popover bell-pop">
            <div className="bell-pop-head">
              <span className="popover-title" style={{ marginBottom: 0 }}>
                Notifications
              </span>
              <div className="spacer" />
              {unread > 0 && (
                <button className="btn btn-ghost btn-sm" onClick={() => void markRead({ all: true })}>
                  Mark all read
                </button>
              )}
            </div>
            {items === null ? (
              <div className="popover-empty">Loading…</div>
            ) : items.length === 0 ? (
              <div className="popover-empty">Nothing yet.</div>
            ) : (
              <ul className="bell-list">
                {items.map((n) => (
                  <li key={n.id} className={`bell-item lvl-${n.level}${n.read_at ? '' : ' unread'}`}>
                    <div className="bell-item-title">{n.title}</div>
                    {n.body && <div className="bell-item-body">{n.body}</div>}
                    <div className="bell-item-meta">
                      <span>{fmtDate(n.created_at)}</span>
                      <span className="mono-sm">{n.event}</span>
                      {n.site_id && (
                        <Link to={`/site/${n.site_id}/jobs`} onClick={() => setOpen(false)}>
                          site jobs
                        </Link>
                      )}
                      {!n.read_at && (
                        <button className="btn-ghost btn-sm bell-read" onClick={() => void markRead({ ids: [n.id] })}>
                          Mark read
                        </button>
                      )}
                    </div>
                  </li>
                ))}
              </ul>
            )}
            <div className="bell-pop-foot">
              <Link to="/settings?section=notifications" onClick={() => setOpen(false)}>
                Notification channels…
              </Link>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
