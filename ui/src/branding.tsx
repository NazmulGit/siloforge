// Public auth config + branding (GET /api/v2/auth/config), loaded once for the whole app.
// The primary colour becomes the CSS variable --brand (auth pages and the logo use it);
// the app name becomes the document title.

import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import { DEFAULT_BRANDING, fetchAuthConfig, type AuthConfig, type Branding } from './saas';

interface BrandCtx {
  config: AuthConfig | null;
  branding: Branding;
  reload: () => Promise<void>;
  /** Live preview from Admin → Branding (not saved). null restores the saved branding. */
  preview: (b: Branding | null) => void;
}

const Ctx = createContext<BrandCtx | null>(null);

const HEX = /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i;

export function safeColor(c: string | null | undefined): string | null {
  const v = (c ?? '').trim();
  return HEX.test(v) ? v : null;
}

export function safeUrl(u: string | null | undefined): string | null {
  const v = (u ?? '').trim();
  if (!v) return null;
  if (v.startsWith('/') && !v.startsWith('//')) return v;
  try {
    const url = new URL(v);
    return url.protocol === 'https:' || url.protocol === 'http:' ? url.href : null;
  } catch {
    return null;
  }
}

function applyBranding(b: Branding): void {
  const root = document.documentElement;
  const color = safeColor(b.primary_color);
  if (color) root.style.setProperty('--brand', color);
  else root.style.removeProperty('--brand');
  document.title = b.app_name?.trim() || 'SiloForge';
}

export function BrandingProvider({ children }: { children: ReactNode }) {
  const [config, setConfig] = useState<AuthConfig | null>(null);
  const [override, setOverride] = useState<Branding | null>(null);

  const reload = useCallback(async () => {
    setConfig(await fetchAuthConfig());
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  const branding = override ?? config?.branding ?? DEFAULT_BRANDING;
  useEffect(() => applyBranding(branding), [branding]);

  const value = useMemo<BrandCtx>(() => ({ config, branding, reload, preview: setOverride }), [config, branding, reload]);
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useBranding(): BrandCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error('useBranding must be used inside <BrandingProvider>');
  return ctx;
}

/** App name split for the two-tone wordmark ("Silo" + "Forge"); custom names render plain. */
export function Wordmark({ name }: { name: string }) {
  if (name === 'SiloForge') {
    return (
      <>
        Silo<b>Forge</b>
      </>
    );
  }
  return <>{name}</>;
}
