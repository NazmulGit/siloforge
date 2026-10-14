/** Chip for edge statuses (suggested/approved/…) and job statuses (queued/running/done/failed). */
export function StatusChip({ status }: { status: string }) {
  return <span className={`chip chip-${status}`}>{status}</span>;
}
