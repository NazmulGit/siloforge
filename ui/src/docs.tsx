// Docs context: the doc index (GET /api/v2/docs, loaded lazily and cached), plus the
// right-side help drawer. openDoc() with no slug opens the doc that matches the current page.

import { createContext, lazy, Suspense, useCallback, useContext, useMemo, useRef, useState, type ReactNode } from 'react';
import { useLocation } from 'react-router-dom';
import { fetchDocs, isUnavailable } from './api2';
import type { DocMeta } from './types2';

const DocDrawer = lazy(() => import('./components/DocDrawer').then((m) => ({ default: m.DocDrawer })));

/**
 * Page key → candidate slugs, most specific first. Slugs come from the server's
 * docs/guide/*.md, so every entry is a guess: the first candidate present in the
 * index wins, then a keyword match on slug/title, then the first doc.
 */
const PAGE_DOCS: Record<string, string[]> = {
  dashboard: ['concepts', 'getting-started'],
  tools: ['review-and-approve', 'inject-and-undo', 'anchors'],
  reports: ['reports'],
  'link-health': ['link-health'],
  history: ['inject-and-undo'],
  integrations: ['sites', 'getting-started'],
  jobs: ['schedules-and-audits', 'crawl'],
  sites: ['sites', 'getting-started'],
  alljobs: ['schedules-and-audits', 'troubleshooting'],
  system: ['resources-and-performance', 'troubleshooting'],
  logs: ['logs'],
  downloads: ['downloads', 'wordpress-plugin'],
  'settings:general': ['getting-started', 'faq'],
  'settings:linking': ['plan-and-silos', 'keyword-rules', 'anchors'],
  'settings:resources': ['resources-and-performance'],
  'settings:wordpress': ['wordpress-plugin', 'sites'],
  'settings:models': ['ai-models'],
  'settings:api': ['api-and-mcp'],
  'settings:notifications': ['notifications'],
  'settings:backups': ['backups-and-migration'],
  'settings:activity': ['logs'],
  'settings:account': ['accounts-and-login', 'faq'],
  'settings:platform': ['admin-console', 'saas-deployment'],
  // v2.4
  workspace: ['workspaces-and-members'],
  'workspace:profile': ['workspaces-and-members'],
  'workspace:members': ['workspaces-and-members'],
  'workspace:plan': ['plans-and-usage', 'workspaces-and-members'],
  'workspace:tokens': ['api-and-mcp', 'workspaces-and-members'],
  'workspace:danger': ['workspaces-and-members'],
  admin: ['admin-console'],
  'admin:overview': ['admin-console'],
  'admin:users': ['admin-console', 'accounts-and-login'],
  'admin:workspaces': ['admin-console', 'workspaces-and-members'],
  'admin:plans': ['plans-and-usage', 'admin-console'],
  'admin:invites': ['workspaces-and-members', 'admin-console'],
  'admin:email': ['admin-console', 'saas-deployment'],
  'admin:registration': ['accounts-and-login', 'admin-console'],
  'admin:branding': ['admin-console'],
  'admin:audit': ['admin-console', 'logs'],
  'admin:deployment': ['saas-deployment', 'admin-console'],
};

export function pageKey(pathname: string, search: string): string {
  const site = /^\/site\/[^/]+\/([\w-]+)/.exec(pathname);
  if (site?.[1]) return site[1];
  if (pathname.startsWith('/settings')) {
    const sec = new URLSearchParams(search).get('section') || 'general';
    return `settings:${sec}`;
  }
  if (pathname === '/jobs') return 'alljobs';
  if (pathname === '/workspace' || pathname === '/admin') {
    const base = pathname.slice(1);
    const tab = new URLSearchParams(search).get('tab');
    return tab ? `${base}:${tab}` : base;
  }
  return pathname.replace(/^\//, '').split('/')[0] || 'sites';
}

export function resolveDocSlug(key: string, docs: DocMeta[]): string | null {
  if (docs.length === 0) return null;
  const slugs = new Set(docs.map((d) => d.slug));
  for (const c of PAGE_DOCS[key] ?? []) if (slugs.has(c)) return c;
  const words = key
    .replace(/^(settings|workspace|admin):/, '')
    .split(/[-:]/)
    .filter((w) => w.length > 2);
  for (const w of words) {
    const hit = docs.find((d) => d.slug.toLowerCase().includes(w) || d.title.toLowerCase().includes(w));
    if (hit) return hit.slug;
  }
  return sortDocs(docs)[0]?.slug ?? null;
}

export function sortDocs(docs: DocMeta[]): DocMeta[] {
  return [...docs].sort(
    (a, b) =>
      (a.section ?? '').localeCompare(b.section ?? '') || (a.order ?? 999) - (b.order ?? 999) || a.title.localeCompare(b.title),
  );
}

interface DocsCtx {
  /** null = not loaded yet; [] with unavailable=true when the server has no docs API. */
  docs: DocMeta[] | null;
  unavailable: boolean;
  error: string | null;
  loadDocs: () => Promise<DocMeta[]>;
  openDoc: (slug?: string) => void;
}

const Ctx = createContext<DocsCtx | null>(null);

export function DocsProvider({ children }: { children: ReactNode }) {
  const [docs, setDocs] = useState<DocMeta[] | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [drawer, setDrawer] = useState<{ slug: string | null } | null>(null);
  const inflight = useRef<Promise<DocMeta[]> | null>(null);
  const location = useLocation();

  const loadDocs = useCallback(async (): Promise<DocMeta[]> => {
    if (docs) return docs;
    if (!inflight.current) {
      inflight.current = fetchDocs()
        .then((d) => {
          setDocs(d);
          return d;
        })
        .catch((e: unknown) => {
          if (isUnavailable(e)) setUnavailable(true);
          else setError(e instanceof Error ? e.message : 'Failed to load docs');
          setDocs([]);
          inflight.current = null;
          return [] as DocMeta[];
        });
    }
    return inflight.current;
  }, [docs]);

  const openDoc = useCallback(
    (slug?: string) => {
      if (slug) {
        setDrawer({ slug });
        return;
      }
      setDrawer({ slug: null });
      const key = pageKey(location.pathname, location.search);
      void loadDocs().then((d) => setDrawer((cur) => (cur && cur.slug === null ? { slug: resolveDocSlug(key, d) } : cur)));
    },
    [loadDocs, location.pathname, location.search],
  );

  const value = useMemo(() => ({ docs, unavailable, error, loadDocs, openDoc }), [docs, unavailable, error, loadDocs, openDoc]);

  return (
    <Ctx.Provider value={value}>
      {children}
      {drawer && (
        <Suspense fallback={null}>
          <DocDrawer slug={drawer.slug} onNavigate={(s) => setDrawer({ slug: s })} onClose={() => setDrawer(null)} />
        </Suspense>
      )}
    </Ctx.Provider>
  );
}

export function useDocs(): DocsCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useDocs must be used inside <DocsProvider>');
  return ctx;
}

/** Small "?" button that opens the doc for the current page (or a given slug). */
export function HelpButton({ slug, label = 'Help for this page' }: { slug?: string; label?: string }) {
  const { openDoc } = useDocs();
  return (
    <button type="button" className="help-btn" aria-label={label} title={label} onClick={() => openDoc(slug)}>
      ?
    </button>
  );
}
