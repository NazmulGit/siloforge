// Global "help hints" mechanism.
//
// A single floating tooltip + one delegated set of document listeners drive
// every hint. You never wrap a control — you just add a `data-hint="..."`
// attribute anywhere in the tree and the provider shows it on hover / focus of
// that element (or its closest ancestor carrying the attribute).
//
// Enabled state persists in localStorage under `sf_help_hints` (default ON).
// When disabled the listeners are removed and nothing renders.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';

const STORAGE_KEY = 'sf_help_hints';
const CURSOR_OFFSET = 14;
const FOCUS_GAP = 6;
const VIEWPORT_MARGIN = 8;

interface HintsCtx {
  enabled: boolean;
  setEnabled: (v: boolean) => void;
}

const Ctx = createContext<HintsCtx | null>(null);

function readInitial(): boolean {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw === null) return true; // feature is opt-out — default ON
    return raw !== '0' && raw !== 'false';
  } catch {
    return true;
  }
}

interface TipState {
  text: string;
  x: number;
  y: number;
  visible: boolean;
}

interface Anchor {
  // Cursor mode follows the mouse; element mode pins under a focused control.
  cursor: boolean;
  x: number;
  y: number;
  // Element-mode extras used when flipping above the control.
  top?: number;
}

function findHint(target: EventTarget | null): { el: Element; text: string } | null {
  if (!(target instanceof Element)) return null;
  const el = target.closest('[data-hint]');
  if (!el) return null;
  const text = el.getAttribute('data-hint');
  if (!text) return null;
  return { el, text };
}

export function HintProvider({ children }: { children: ReactNode }) {
  const [enabled, setEnabledState] = useState<boolean>(readInitial);
  const [tip, setTip] = useState<TipState>({ text: '', x: 0, y: 0, visible: false });
  const tipRef = useRef<HTMLDivElement>(null);
  const activeElRef = useRef<Element | null>(null);
  const anchorRef = useRef<Anchor | null>(null);

  const setEnabled = useCallback((v: boolean) => {
    setEnabledState(v);
    try {
      localStorage.setItem(STORAGE_KEY, v ? '1' : '0');
    } catch {
      /* storage unavailable — keep in-memory state */
    }
  }, []);

  // Clamp a desired anchor to a viewport-safe top-left, flipping when it would overflow.
  const computePos = useCallback((a: Anchor): { x: number; y: number } => {
    const el = tipRef.current;
    const w = el?.offsetWidth ?? 240;
    const h = el?.offsetHeight ?? 44;
    const vw = window.innerWidth;
    const vh = window.innerHeight;

    let x = a.cursor ? a.x + CURSOR_OFFSET : a.x;
    let y = a.cursor ? a.y + CURSOR_OFFSET : a.y + FOCUS_GAP;

    if (x + w + VIEWPORT_MARGIN > vw) x = a.cursor ? a.x - CURSOR_OFFSET - w : vw - w - VIEWPORT_MARGIN;
    if (x < VIEWPORT_MARGIN) x = VIEWPORT_MARGIN;

    if (y + h + VIEWPORT_MARGIN > vh) {
      y = a.cursor ? a.y - CURSOR_OFFSET - h : (a.top ?? a.y) - h - FOCUS_GAP;
    }
    if (y < VIEWPORT_MARGIN) y = VIEWPORT_MARGIN;

    return { x, y };
  }, []);

  useEffect(() => {
    if (!enabled) {
      activeElRef.current = null;
      anchorRef.current = null;
      setTip((t) => (t.visible ? { ...t, visible: false } : t));
      return;
    }

    const hide = () => {
      activeElRef.current = null;
      anchorRef.current = null;
      setTip((t) => (t.visible ? { ...t, visible: false } : t));
    };

    const showCursor = (text: string, el: Element, clientX: number, clientY: number) => {
      activeElRef.current = el;
      const anchor: Anchor = { cursor: true, x: clientX, y: clientY };
      anchorRef.current = anchor;
      const { x, y } = computePos(anchor);
      setTip({ text, x, y, visible: true });
    };

    const onOver = (e: MouseEvent) => {
      const hit = findHint(e.target);
      if (!hit) return;
      showCursor(hit.text, hit.el, e.clientX, e.clientY);
    };

    const onMove = (e: MouseEvent) => {
      if (!activeElRef.current) return;
      const hit = findHint(e.target);
      if (!hit) {
        hide();
        return;
      }
      // Following the cursor; onOver already handled switching to a new hint.
      showCursor(hit.text, hit.el, e.clientX, e.clientY);
    };

    const onOut = (e: MouseEvent) => {
      if (!activeElRef.current) return;
      const to = e.relatedTarget;
      if (to instanceof Node && activeElRef.current.contains(to)) return; // still inside
      hide();
    };

    const onFocusIn = (e: FocusEvent) => {
      const hit = findHint(e.target);
      if (!hit) return;
      activeElRef.current = hit.el;
      const r = hit.el.getBoundingClientRect();
      const anchor: Anchor = { cursor: false, x: r.left, y: r.bottom, top: r.top };
      anchorRef.current = anchor;
      const { x, y } = computePos(anchor);
      setTip({ text: hit.text, x, y, visible: true });
    };

    const onFocusOut = () => hide();
    const onScroll = () => hide();

    document.addEventListener('mouseover', onOver);
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseout', onOut);
    document.addEventListener('focusin', onFocusIn);
    document.addEventListener('focusout', onFocusOut);
    window.addEventListener('scroll', onScroll, true);

    return () => {
      document.removeEventListener('mouseover', onOver);
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseout', onOut);
      document.removeEventListener('focusin', onFocusIn);
      document.removeEventListener('focusout', onFocusOut);
      window.removeEventListener('scroll', onScroll, true);
    };
  }, [enabled, computePos]);

  // Re-clamp once the tooltip's real size is known (first paint of new text can
  // otherwise overflow because computePos measured the previous content).
  useLayoutEffect(() => {
    if (!tip.visible || !anchorRef.current) return;
    const { x, y } = computePos(anchorRef.current);
    if (Math.abs(x - tip.x) > 0.5 || Math.abs(y - tip.y) > 0.5) {
      setTip((t) => ({ ...t, x, y }));
    }
  }, [tip.visible, tip.text, tip.x, tip.y, computePos]);

  const value = useMemo<HintsCtx>(() => ({ enabled, setEnabled }), [enabled, setEnabled]);

  return (
    <Ctx.Provider value={value}>
      {children}
      {enabled && (
        <div
          ref={tipRef}
          className={`hint-tip${tip.visible ? ' visible' : ''}`}
          role="tooltip"
          aria-hidden={!tip.visible}
          style={{ left: tip.x, top: tip.y }}
        >
          {tip.text}
        </div>
      )}
    </Ctx.Provider>
  );
}

export function useHints(): HintsCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useHints must be used inside <HintProvider>');
  return ctx;
}
