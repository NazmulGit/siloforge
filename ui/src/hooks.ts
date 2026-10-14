import { useCallback, useEffect, useRef } from 'react';
import { ApiError, fetchJobRun, fetchJobs } from './api';
import { useToast } from './toast';
import type { JobRun } from './types';

const POLL_MS = 4000;
/** Hard cap on how long one tracker keeps polling (long crawls stay visible in Jobs). */
const MAX_TRACK_MS = 6 * 60 * 60 * 1000;
/** Give up early if the run cannot be found at all for this long. */
const NOT_FOUND_GIVE_UP_MS = 2 * 60 * 1000;

function isTerminal(job: JobRun): boolean {
  return job.status === 'done' || job.status === 'failed' || job.status === 'cancelled';
}

/**
 * Tracks a triggered job_run by polling GET /jobs/run/:id every 4s until the
 * run reaches done/failed/cancelled, then toasts and invokes onDone. Against an
 * older server without that endpoint (404) it falls back to scanning
 * GET /jobs/:siteId. Polling stops after MAX_TRACK_MS, or after
 * NOT_FOUND_GIVE_UP_MS if the run never shows up. Timers are cleaned up on unmount.
 */
export function useJobTracker(): (
  siteId: string,
  jobRunId: string,
  label: string,
  onDone?: (job: JobRun) => void,
) => void {
  const toast = useToast();
  const timers = useRef<number[]>([]);

  useEffect(
    () => () => {
      timers.current.forEach((t) => window.clearInterval(t));
      timers.current = [];
    },
    [],
  );

  return useCallback(
    (siteId: string, jobRunId: string, label: string, onDone?: (job: JobRun) => void) => {
      toast.push(`${label} queued`, 'info');
      const startedAt = Date.now();
      let lastSeenAt = startedAt;
      let useList = false; // true once /jobs/run/:id answered 404
      let inFlight = false;

      const stop = () => {
        window.clearInterval(interval);
        timers.current = timers.current.filter((t) => t !== interval);
      };

      const lookup = async (): Promise<JobRun | null> => {
        if (!useList) {
          try {
            return await fetchJobRun(jobRunId);
          } catch (e) {
            if (!(e instanceof ApiError && e.status === 404)) throw e;
            useList = true;
          }
        }
        const jobs = await fetchJobs(siteId);
        return jobs.find((j) => j.id === jobRunId) ?? null;
      };

      const interval = window.setInterval(() => {
        if (inFlight) return;
        inFlight = true;
        void (async () => {
          try {
            const now = Date.now();
            if (now - startedAt > MAX_TRACK_MS) {
              stop();
              toast.push(`${label} is still running — check the Jobs page for progress`, 'info');
              return;
            }
            let job: JobRun | null;
            try {
              job = await lookup();
            } catch {
              return; // transient — keep polling
            }
            if (!job) {
              if (now - lastSeenAt > NOT_FOUND_GIVE_UP_MS) {
                stop();
                toast.push(`Lost track of ${label.toLowerCase()} — see the Jobs page`, 'info');
              }
              return;
            }
            lastSeenAt = now;
            if (!isTerminal(job)) return;
            stop();
            if (job.status === 'done') {
              toast.push(`${label} finished`, 'success');
            } else if (job.status === 'cancelled') {
              toast.push(`${label} cancelled`, 'info');
            } else {
              toast.push(`${label} failed${job.error ? `: ${job.error}` : ''}`, 'error');
            }
            onDone?.(job);
          } finally {
            inFlight = false;
          }
        })();
      }, POLL_MS);
      timers.current.push(interval);
    },
    [toast],
  );
}
