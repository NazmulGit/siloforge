// Clickable + copyable post links used everywhere a post title / URL is shown.
//
// These are REAL anchors, so the browser gives us both behaviours for free:
//   • left-click  → opens the post in a new tab
//   • right-click → native "Copy link address" (the copyable requirement)
// No custom JS needed. `data-hint` plugs into the global help-hints system.

const OPEN_HINT = 'Open this post in a new tab (right-click to copy the link)';

export interface PostLinkProps {
  url: string;
  /** Visible label. Falls back to the URL when omitted or empty. */
  title?: string;
  className?: string;
}

/** A post title (or URL) rendered as a real, clickable, copyable link. */
export function PostLink({ url, title, className }: PostLinkProps) {
  const label = title != null && title !== '' ? title : url;
  return (
    <a
      href={url}
      target="_blank"
      rel="noopener noreferrer"
      className={`postlink${className ? ` ${className}` : ''}`}
      title={label}
      data-hint={OPEN_HINT}
    >
      {label}
    </a>
  );
}

export interface UrlMonoProps {
  url: string;
  className?: string;
}

/** The monospace URL variant for URL columns — still a real clickable link. */
export function UrlMono({ url, className }: UrlMonoProps) {
  return (
    <a
      href={url}
      target="_blank"
      rel="noopener noreferrer"
      className={`urlmono${className ? ` ${className}` : ''}`}
      title={url}
      data-hint={OPEN_HINT}
    >
      {url}
    </a>
  );
}
