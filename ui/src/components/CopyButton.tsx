import { useState } from 'react';
import { useToast } from '../toast';

/** Copies `text` to the clipboard; shows "Copied" briefly. */
export function CopyButton({ text, label = 'Copy', what = 'Text' }: { text: string; label?: string; what?: string }) {
  const toast = useToast();
  const [done, setDone] = useState(false);
  return (
    <button
      type="button"
      className="btn btn-sm"
      aria-label={`Copy ${what.toLowerCase()}`}
      onClick={() => {
        void navigator.clipboard.writeText(text).then(
          () => {
            setDone(true);
            window.setTimeout(() => setDone(false), 1500);
          },
          () => toast.push('Copy failed — select the text manually', 'error'),
        );
      }}
    >
      {done ? 'Copied' : label}
    </button>
  );
}
