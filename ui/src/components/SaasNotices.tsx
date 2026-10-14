// App-wide v2.4 notices: the plan-limit upgrade prompt (any 402), the verify-email banner
// (403 "email not verified", or an unverified account while verification is required),
// and the read-only "view as" banner for platform admins.

import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { useBranding } from '../branding';
import { LIMIT_LABELS, resendVerification } from '../saas';
import { useToast } from '../toast';
import { useUser } from '../user';
import { useWorkspace } from '../workspaces';
import { onEmailUnverified, onPlanLimit, type PlanLimitDetail } from '../wsStore';
import { Modal } from './Modal';

function fmtLimit(n: number | null): string {
  return n === null ? 'unlimited' : n.toLocaleString();
}

/** Human text for a plan-limit hit; exported for inline use (e.g. a disabled button's note). */
export function planLimitText(d: PlanLimitDetail, supportEmail: string): string {
  const label = LIMIT_LABELS[d.key] ?? d.key.replace(/_/g, ' ');
  // "1 site", "1 member": singular for the countable keys.
  const what = d.limit === 1 && ['sites', 'members', 'api_tokens', 'schedules'].includes(d.key) ? label.replace(/s$/, '') : label;
  const plan = d.plan ? d.plan.charAt(0).toUpperCase() + d.plan.slice(1) : 'current';
  const used = d.used === null ? '' : `; you have used ${d.used.toLocaleString()}`;
  const contact = supportEmail ? ` Contact ${supportEmail} to upgrade.` : ' Ask your platform admin to upgrade.';
  if (d.limit === 0 || (d.limit === null && typeof d.used !== 'number')) return `Your ${plan} plan does not include ${what}.${contact}`;
  return `Your ${plan} plan allows ${fmtLimit(d.limit)} ${what}${used}.${contact}`;
}

export function PlanLimitHost() {
  const { branding } = useBranding();
  const [hit, setHit] = useState<PlanLimitDetail | null>(null);
  useEffect(() => onPlanLimit(setHit), []);
  if (!hit) return null;
  const support = branding.support_email.trim();
  return (
    <Modal
      title="Plan limit reached"
      onClose={() => setHit(null)}
      footer={
        <>
          <Link className="btn" to="/workspace?tab=plan" onClick={() => setHit(null)}>
            See plan & usage
          </Link>
          {support && (
            <a className="btn btn-primary" href={`mailto:${support}?subject=${encodeURIComponent('Plan upgrade')}`}>
              Email {support}
            </a>
          )}
          {!support && (
            <button className="btn btn-primary" onClick={() => setHit(null)}>
              OK
            </button>
          )}
        </>
      }
    >
      <div className="plan-limit">
        <div className="plan-limit-meter" aria-hidden="true">
          <span style={{ width: '100%' }} />
        </div>
        <p>{planLimitText(hit, support)}</p>
        {hit.message && hit.message !== 'plan limit' && <p className="muted">{hit.message}</p>}
      </div>
    </Modal>
  );
}

export function VerifyEmailBanner() {
  const { user } = useUser();
  const { config } = useBranding();
  const toast = useToast();
  const [forced, setForced] = useState(false);
  const [busy, setBusy] = useState(false);
  const [link, setLink] = useState(false);
  useEffect(() => onEmailUnverified(() => setForced(true)), []);

  const unverified = user?.email_verified === false;
  const show = unverified && (forced || config?.require_email_verification === true);
  if (!show) return null;

  async function resend() {
    setBusy(true);
    try {
      const r = await resendVerification();
      if (r.delivery === 'manual') {
        setLink(true);
        toast.push('No mail server is set up: ask your platform admin for the link.', 'info');
      } else toast.push(`Verification email sent to ${user?.email ?? 'you'}`, 'success');
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Could not send the email', 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="app-banner banner-verify" role="status">
      <span>
        <b>Confirm your email address.</b> Open the link we sent to {user?.email}. Adding sites stays locked until then.
        {link && ' Your platform admin can copy the link from Admin, Email.'}
      </span>
      <button className="btn btn-sm" disabled={busy} onClick={() => void resend()}>
        {busy ? 'Sending…' : 'Resend email'}
      </button>
    </div>
  );
}

export function ViewAsBanner() {
  const { viewAs, exitViewAs } = useWorkspace();
  if (!viewAs) return null;
  return (
    <div className="app-banner banner-viewas" role="status">
      <span>
        Viewing <b>{viewAs.name}</b> as support (read-only). Changes are refused and every request is logged.
      </span>
      <button className="btn btn-sm" onClick={exitViewAs}>
        Exit
      </button>
    </div>
  );
}
