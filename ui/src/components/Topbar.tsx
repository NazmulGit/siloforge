import { useNavigate } from 'react-router-dom';
import { useHints } from '../hints';
import { useToast } from '../toast';
import { useUser } from '../user';
import { NotificationBell } from './NotificationBell';
import { SiteSelect } from './SiteSelect';

export function Topbar({ siteId }: { siteId?: string }) {
  const navigate = useNavigate();
  const { user, logout } = useUser();
  const { enabled, setEnabled } = useHints();
  const toast = useToast();
  return (
    <header className="topbar">
      <SiteSelect siteId={siteId} />
      <div className="spacer" />
      {/* notifications are admin-only on the server (A4) */}
      {user?.role === 'admin' && <NotificationBell />}
      <button
        type="button"
        className={`help-toggle${enabled ? ' on' : ''}`}
        aria-pressed={enabled}
        aria-label="Toggle help hints"
        data-hint="Toggle these hover help hints on or off. You can also change this in Settings."
        onClick={() => {
          const next = !enabled;
          setEnabled(next);
          toast.push(next ? 'Help hints on' : 'Help hints off', 'info');
        }}
      >
        ?
      </button>
      {user && (
        <span className="topbar-user" style={{ display: 'inline-flex', alignItems: 'center', gap: 8, marginRight: 12 }}>
          <span style={{ opacity: 0.85 }}>{user.email}</span>
          <span
            className={`role-chip role-${user.role}`}
            style={{
              fontSize: 11,
              fontWeight: 600,
              textTransform: 'uppercase',
              letterSpacing: '0.05em',
              padding: '2px 8px',
              borderRadius: 999,
              border: '1px solid',
              borderColor: user.role === 'admin' ? '#2f6bff' : '#64748b',
              color: user.role === 'admin' ? '#2f6bff' : '#5b6a82',
            }}
          >
            {user.role}
          </span>
        </span>
      )}
      <button
        className="btn btn-ghost"
        data-hint="Signs you out and clears your session token from this browser. No data is changed."
        onClick={() => {
          logout();
          navigate('/login');
        }}
      >
        Log out
      </button>
    </header>
  );
}
