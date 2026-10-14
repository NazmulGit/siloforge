// Shared frame for the sign-in family of pages (login, 2FA, register, forgot, reset, verify,
// invite). Left: an ink panel with a small silo map, the thing SiloForge builds: pillar
// pages with their supporting posts, linked inside each silo and across by one bridge.
// Right: the form, left-aligned on paper. Branding (name, logo, colour, legal links)
// comes from GET /api/v2/auth/config.

import { useState, type ReactNode } from 'react';
import { safeUrl, useBranding, Wordmark } from '../../branding';

interface Node {
  id: string;
  x: number;
  y: number;
  pillar?: boolean;
}

// Three silos. Coordinates are fixed so the drawing is the same on every load.
const NODES: Node[] = [
  { id: 'a', x: 92, y: 118, pillar: true },
  { id: 'a1', x: 40, y: 64 },
  { id: 'a2', x: 150, y: 58 },
  { id: 'a3', x: 34, y: 172 },
  { id: 'a4', x: 128, y: 190 },
  { id: 'b', x: 268, y: 214, pillar: true },
  { id: 'b1', x: 222, y: 150 },
  { id: 'b2', x: 330, y: 160 },
  { id: 'b3', x: 316, y: 276 },
  { id: 'b4', x: 214, y: 280 },
  { id: 'c', x: 128, y: 344, pillar: true },
  { id: 'c1', x: 60, y: 300 },
  { id: 'c2', x: 64, y: 400 },
  { id: 'c3', x: 186, y: 410 },
];

const EDGES: Array<[string, string, 'in' | 'bridge']> = [
  ['a', 'a1', 'in'],
  ['a', 'a2', 'in'],
  ['a', 'a3', 'in'],
  ['a', 'a4', 'in'],
  ['a1', 'a2', 'in'],
  ['b', 'b1', 'in'],
  ['b', 'b2', 'in'],
  ['b', 'b3', 'in'],
  ['b', 'b4', 'in'],
  ['b2', 'b3', 'in'],
  ['c', 'c1', 'in'],
  ['c', 'c2', 'in'],
  ['c', 'c3', 'in'],
  ['a4', 'b1', 'bridge'],
  ['b4', 'c3', 'bridge'],
];

function SiloMap() {
  const at = new Map(NODES.map((n) => [n.id, n]));
  return (
    <svg className="silo-map" viewBox="0 0 380 450" role="img" aria-label="Three topic silos: pillar pages linked to their posts">
      <g className="silo-edges">
        {EDGES.map(([from, to, kind], i) => {
          const a = at.get(from);
          const b = at.get(to);
          if (!a || !b) return null;
          return (
            <line
              key={`${from}-${to}`}
              x1={a.x}
              y1={a.y}
              x2={b.x}
              y2={b.y}
              className={`silo-edge ${kind}`}
              pathLength={1}
              style={{ animationDelay: `${120 + i * 70}ms` }}
            />
          );
        })}
      </g>
      {NODES.map((n) => (
        <circle
          key={n.id}
          cx={n.x}
          cy={n.y}
          r={n.pillar ? 11 : 5.5}
          className={`silo-node${n.pillar ? ' pillar' : ''}${n.id === 'b' ? ' here' : ''}`}
        />
      ))}
    </svg>
  );
}

function Logo() {
  const { branding } = useBranding();
  const [broken, setBroken] = useState(false);
  const logo = safeUrl(branding.logo_url);
  const name = branding.app_name.trim() || 'SiloForge';
  return (
    <div className="auth-brand">
      {logo && !broken ? (
        <img src={logo} alt="" className="auth-logo-img" onError={() => setBroken(true)} />
      ) : (
        <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
          <path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71" />
          <path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71" />
        </svg>
      )}
      <span className="auth-wordmark">
        <Wordmark name={name} />
      </span>
    </div>
  );
}

export function AuthShell({ title, lead, children, wide }: { title: string; lead?: ReactNode; children: ReactNode; wide?: boolean }) {
  const { branding } = useBranding();
  const terms = safeUrl(branding.terms_url);
  const privacy = safeUrl(branding.privacy_url);
  const support = branding.support_email.trim();
  return (
    <div className="auth">
      <aside className="auth-art" aria-hidden="false">
        <Logo />
        <SiloMap />
        <p className="auth-art-line">Plan, review and place internal links across your WordPress sites. Every change can be undone.</p>
      </aside>
      <main className="auth-main">
        <div className={`auth-panel${wide ? ' wide' : ''}`}>
          <h1 className="auth-title">{title}</h1>
          {lead && <div className="auth-lead">{lead}</div>}
          {children}
        </div>
        {(terms || privacy || support) && (
          <footer className="auth-legal">
            {terms && (
              <a href={terms} target="_blank" rel="noreferrer">
                Terms
              </a>
            )}
            {privacy && (
              <a href={privacy} target="_blank" rel="noreferrer">
                Privacy
              </a>
            )}
            {support && <a href={`mailto:${support}`}>{support}</a>}
          </footer>
        )}
      </main>
    </div>
  );
}

export function PasswordField({
  label,
  value,
  onChange,
  autoComplete,
  autoFocus,
  placeholder,
  note,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  autoComplete: string;
  autoFocus?: boolean;
  placeholder?: string;
  note?: string;
}) {
  const [shown, setShown] = useState(false);
  return (
    <label className="field auth-field">
      {label}
      <span className="pw-wrap">
        <input
          type={shown ? 'text' : 'password'}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          autoComplete={autoComplete}
          autoFocus={autoFocus}
          placeholder={placeholder}
          spellCheck={false}
        />
        <button
          type="button"
          className="pw-toggle"
          aria-label={shown ? 'Hide password' : 'Show password'}
          aria-pressed={shown}
          onClick={(e) => {
            e.preventDefault();
            setShown((v) => !v);
          }}
        >
          {shown ? 'Hide' : 'Show'}
        </button>
      </span>
      {note && <span className="field-note faint">{note}</span>}
    </label>
  );
}

/** Small "loading" body while the auth config arrives. */
export function AuthLoading() {
  return (
    <div className="auth-loading" role="status">
      Loading…
    </div>
  );
}
