// Colour theme: 'system' follows the OS; 'light'/'dark' force it via <html data-theme>.

export type Theme = 'system' | 'light' | 'dark';
const KEY = 'sf_theme';

export function readTheme(): Theme {
  try {
    const v = localStorage.getItem(KEY);
    return v === 'light' || v === 'dark' ? v : 'system';
  } catch {
    return 'system';
  }
}

export function applyTheme(t: Theme): void {
  const el = document.documentElement;
  if (t === 'system') delete el.dataset.theme;
  else el.dataset.theme = t;
}

export function saveTheme(t: Theme): void {
  try {
    if (t === 'system') localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, t);
  } catch {
    /* storage unavailable — ignore */
  }
  applyTheme(t);
}
