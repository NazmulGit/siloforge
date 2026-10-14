import { useState, type ReactNode } from 'react';
import { absoluteLink } from '../../format';
import { CopyButton } from '../CopyButton';
import { Modal } from '../Modal';

export function errText(e: unknown, fallback: string): string {
  return e instanceof Error && e.message ? e.message : fallback;
}

/** Shows a one-time link (manual delivery) with a copy button. */
export function LinkModal({ title, text, link, onClose }: { title: string; text: ReactNode; link: string; onClose: () => void }) {
  const full = absoluteLink(link);
  return (
    <Modal
      title={title}
      onClose={onClose}
      footer={
        <button className="btn btn-primary" onClick={onClose}>
          Done
        </button>
      }
    >
      <p className="muted" style={{ marginTop: 0 }}>
        {text}
      </p>
      <div className="apikey-box">
        <code>{full}</code>
        <CopyButton text={full} what="Link" />
      </div>
    </Modal>
  );
}

/** Search box + page controls shared by the admin lists. */
export function Pager({ offset, limit, total, onPage }: { offset: number; limit: number; total: number; onPage: (offset: number) => void }) {
  if (total <= limit) return null;
  const from = offset + 1;
  const to = Math.min(total, offset + limit);
  return (
    <div className="pager">
      <span className="faint">
        {from.toLocaleString()} to {to.toLocaleString()} of {total.toLocaleString()}
      </span>
      <button className="btn btn-sm" disabled={offset === 0} onClick={() => onPage(Math.max(0, offset - limit))}>
        Previous
      </button>
      <button className="btn btn-sm" disabled={to >= total} onClick={() => onPage(offset + limit)}>
        Next
      </button>
    </div>
  );
}

/** Debounced search input. */
export function useDebounced(initial = ''): [string, string, (v: string) => void] {
  const [input, setInput] = useState(initial);
  const [value, setValue] = useState(initial);
  const [timer, setTimer] = useState<number | null>(null);
  const set = (v: string) => {
    setInput(v);
    if (timer) window.clearTimeout(timer);
    setTimer(window.setTimeout(() => setValue(v.trim()), 300));
  };
  return [input, value, set];
}
