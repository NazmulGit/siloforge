// Link Run status helpers shared by the stepper, the history list and the Auto-Linker card.
import type { LinkRun, LinkRunItem } from '../../types2';

export const ACTIVE_STATUSES = new Set(['crawling', 'planning', 'deploying']);
export const DEPLOYED_STATUSES = new Set(['deployed', 'partially_deployed']);

export function isActive(run: LinkRun | null | undefined): boolean {
  return !!run && ACTIVE_STATUSES.has(run.status);
}

export const RUN_STATUS_LABEL: Record<string, string> = {
  draft: 'Draft',
  crawling: 'Crawling',
  planning: 'Planning',
  ready: 'Ready to deploy',
  deploying: 'Deploying',
  deployed: 'Deployed',
  partially_deployed: 'Partly deployed',
  failed: 'Failed',
  cancelled: 'Cancelled',
  undone: 'Undone',
};

/** Tone class suffix for a run status. */
export function runTone(status: string): 'live' | 'good' | 'warn' | 'bad' | 'idle' {
  if (ACTIVE_STATUSES.has(status)) return 'live';
  if (status === 'deployed' || status === 'ready') return 'good';
  if (status === 'partially_deployed' || status === 'undone') return 'warn';
  if (status === 'failed') return 'bad';
  return 'idle';
}

export function RunBadge({ status }: { status: string }) {
  return <span className={`lr-run-badge lr-tone-${runTone(status)}`}>{RUN_STATUS_LABEL[status] ?? status}</span>;
}

export const ITEM_STATUS_LABEL: Record<string, string> = {
  suggested: 'Not selected',
  selected: 'Selected',
  applied: 'Applied',
  failed: 'Failed',
  not_present: 'Not present',
  skipped: 'Skipped',
  reverted: 'Reverted',
};

export function ItemBadge({ item }: { item: LinkRunItem }) {
  const unverified = item.status === 'applied' && item.verify?.present === false;
  const key = unverified ? 'not_present' : item.status;
  return (
    <span className={`lr-item-badge lr-item-${key}`} title={unverified ? 'Applied, but the live page does not show it' : undefined}>
      {unverified ? 'Not on live page' : (ITEM_STATUS_LABEL[key] ?? key)}
    </span>
  );
}

/** An item that "Apply properly" can work on: failed, not present, or applied but missing on the live page. */
export function needsFix(item: LinkRunItem): boolean {
  return (
    item.status === 'failed' ||
    item.status === 'not_present' ||
    (item.status === 'applied' && item.verify?.present === false)
  );
}

/** URL that opens the live post scrolled to the link (server `open_url`, else a text fragment we build). */
export function openUrl(item: LinkRunItem): string {
  if (item.open_url) return item.open_url;
  const base = (item.live_url || item.source.url || '').split('#')[0] ?? '';
  if (!base || !item.anchor) return base;
  const frag = encodeURIComponent(item.anchor).replace(/-/g, '%2D');
  return `${base}#:~:text=${frag}`;
}

/** Which of the 9 steps a run belongs on right now. */
export function stepForRun(run: LinkRun): number {
  switch (run.status) {
    case 'draft':
    case 'crawling':
    case 'planning':
      return 5;
    case 'ready':
      return 6;
    case 'deploying':
      return 7;
    case 'deployed':
    case 'partially_deployed':
    case 'undone':
      return 8;
    case 'failed':
    case 'cancelled':
      return run.deployed_at || run.deploy_job_id ? 8 : 5;
    default:
      return 5;
  }
}

export function n(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : 0;
}
