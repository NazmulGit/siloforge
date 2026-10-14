// Current workspace + admin "view as" state, kept in localStorage so every request
// (api.ts authHeaders) can send X-SiloForge-Workspace / X-SiloForge-View-As without
// importing React state. No imports here on purpose: api.ts depends on this file.

export const WORKSPACE_KEY = 'sf_workspace';
export const VIEW_AS_KEY = 'sf_view_as';
/** Stored by platform admins who chose "All workspaces": no workspace header is sent. */
export const ALL_WORKSPACES = '__all__';

export interface ViewAs {
  id: string;
  name: string;
}

function get(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function set(key: string, value: string | null): void {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    /* storage unavailable — ignore */
  }
}

export function readWorkspaceId(): string | null {
  const v = get(WORKSPACE_KEY);
  return v && v.trim() ? v : null;
}

export function writeWorkspaceId(id: string | null): void {
  set(WORKSPACE_KEY, id);
}

export function readViewAs(): ViewAs | null {
  const raw = get(VIEW_AS_KEY);
  if (!raw) return null;
  try {
    const v = JSON.parse(raw) as Partial<ViewAs>;
    return typeof v.id === 'string' && v.id ? { id: v.id, name: typeof v.name === 'string' ? v.name : v.id } : null;
  } catch {
    return null;
  }
}

export function writeViewAs(v: ViewAs | null): void {
  set(VIEW_AS_KEY, v ? JSON.stringify(v) : null);
}

/** Headers that scope a request to the current workspace (and the support view, when on). */
export function scopeHeaders(): Record<string, string> {
  const h: Record<string, string> = {};
  const view = readViewAs();
  if (view) {
    h['X-SiloForge-View-As'] = view.id;
    h['X-SiloForge-Workspace'] = view.id;
    return h;
  }
  const ws = readWorkspaceId();
  if (ws && ws !== ALL_WORKSPACES) h['X-SiloForge-Workspace'] = ws;
  return h;
}

// ---- cross-cutting UI events raised by the fetch wrapper ----

export interface PlanLimitDetail {
  key: string;
  limit: number | null;
  used: number | null;
  plan: string;
  workspace_id?: string;
  message?: string;
}

const PLAN_EVENT = 'sf:plan-limit';
const VERIFY_EVENT = 'sf:email-unverified';

export function emitPlanLimit(d: PlanLimitDetail): void {
  window.dispatchEvent(new CustomEvent<PlanLimitDetail>(PLAN_EVENT, { detail: d }));
}

export function onPlanLimit(cb: (d: PlanLimitDetail) => void): () => void {
  const h = (e: Event) => cb((e as CustomEvent<PlanLimitDetail>).detail);
  window.addEventListener(PLAN_EVENT, h);
  return () => window.removeEventListener(PLAN_EVENT, h);
}

export function emitEmailUnverified(): void {
  window.dispatchEvent(new Event(VERIFY_EVENT));
}

export function onEmailUnverified(cb: () => void): () => void {
  window.addEventListener(VERIFY_EVENT, cb);
  return () => window.removeEventListener(VERIFY_EVENT, cb);
}
