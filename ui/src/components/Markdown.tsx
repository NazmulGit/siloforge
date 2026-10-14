import DOMPurify from 'dompurify';
import { marked } from 'marked';
import { useEffect, useMemo, useRef } from 'react';

marked.setOptions({ gfm: true, breaks: false });

function slugify(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^\w\s-]/g, '')
    .trim()
    .replace(/\s+/g, '-');
}

/** Strip a leading YAML front-matter block, if the server left one in. */
function stripFrontMatter(md: string): string {
  return md.replace(/^---\r?\n[\s\S]*?\r?\n---\r?\n?/, '');
}

/** Link target inside the guide: `other.md`, `./other`, `other.md#part`, or `/docs/other`. */
export function docSlugFromHref(href: string): string | null {
  if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith('//') || href.startsWith('#')) return null;
  const m = /^(?:\.\/|\/docs\/)?([\w-]+?)(?:\.md)?(?:#.*)?$/.exec(href);
  return m?.[1] ?? null;
}

/**
 * Renders guide markdown (marked → DOMPurify). Headings get ids, external links open
 * in a new tab, and links to other guide pages call onDocLink instead of navigating.
 */
export function Markdown({ source, onDocLink }: { source: string; onDocLink?: (slug: string) => void }) {
  const ref = useRef<HTMLDivElement>(null);
  const html = useMemo(() => {
    const raw = marked.parse(stripFrontMatter(source), { async: false });
    return DOMPurify.sanitize(typeof raw === 'string' ? raw : '', { USE_PROFILES: { html: true } });
  }, [source]);

  useEffect(() => {
    const root = ref.current;
    if (!root) return;
    root.querySelectorAll('h1, h2, h3, h4').forEach((h) => {
      if (!h.id) h.id = slugify(h.textContent ?? '');
    });
    root.querySelectorAll('a[href]').forEach((a) => {
      const href = a.getAttribute('href') ?? '';
      if (/^https?:/i.test(href)) {
        a.setAttribute('target', '_blank');
        a.setAttribute('rel', 'noopener noreferrer');
      }
    });
    root.querySelectorAll('table').forEach((t) => {
      if (t.parentElement?.classList.contains('md-table')) return;
      const wrap = document.createElement('div');
      wrap.className = 'md-table';
      t.replaceWith(wrap);
      wrap.appendChild(t);
    });
  }, [html]);

  return (
    <div
      ref={ref}
      className="md"
      onClick={(e) => {
        const a = (e.target as HTMLElement).closest('a');
        if (!a) return;
        const href = a.getAttribute('href') ?? '';
        if (href.startsWith('#')) {
          e.preventDefault();
          ref.current?.querySelector(`#${CSS.escape(href.slice(1))}`)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
          return;
        }
        const slug = docSlugFromHref(href);
        if (slug && onDocLink) {
          e.preventDefault();
          onDocLink(slug);
        }
      }}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
