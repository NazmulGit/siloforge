import { useNavigate } from 'react-router-dom';
import { useDocs } from '../docs';
import { useHints } from '../hints';
import { useToast } from '../toast';
import { useUser } from '../user';
import { Icon } from './Icon';
import { NotificationBell } from './NotificationBell';
import { SiteSelect } from './SiteSelect';

export function Topbar({ onMenu }: { onMenu: () => void }) {
  const navigate = useNavigate();
  const { user, logout } = useUser();
  const { enabled, setEnabled } = useHints();
  const { openDoc } = useDocs();
  const toast = useToast();
  return (
    <header className="topbar">
      <button type="button" className="icon-btn menu-btn" aria-label="Open menu" onClick={onMenu}>
        <Icon name="menu" size={18} />
      </button>
      <SiteSelect />
      <div className="spacer" />
      {/* notifications are admin-only on the server (A4) */}
      {user?.role === 'admin' && <NotificationBell />}
      <button
        type="button"
        className={`icon-btn hint-btn${enabled ? ' on' : ''}`}
        aria-pressed={enabled}
        aria-label="Toggle hover hints"
        title={enabled ? 'Hover hints on' : 'Hover hints off'}
        data-hint="Toggle these hover help hints on or off. You can also change this in Settings."
        onClick={() => {
          const next = !enabled;
          setEnabled(next);
          toast.push(next ? 'Help hints on' : 'Help hints off', 'info');
        }}
      >
        <Icon name="bulb" size={17} />
      </button>
      <button
        type="button"
        className="icon-btn help-open"
        aria-label="Open help for this page"
        title="Help for this page"
        data-hint="Opens the user guide page for what you are looking at, in a side panel."
        onClick={() => openDoc()}
      >
        <Icon name="help" size={17} />
      </button>
      {user && (
        <span className="topbar-user">
          <span className="topbar-email">{user.email}</span>
          <span className={`role-chip role-${user.role}`}>{user.role}</span>
        </span>
      )}
      <button
        className="btn btn-ghost btn-sm"
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
