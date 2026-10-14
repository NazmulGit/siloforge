// Tiny stroke icon set (24x24 grid, currentColor). No icon font, no dependency.

const PATHS: Record<string, string[]> = {
  dashboard: ['M3 13h8V3H3z', 'M13 21h8V11h-8z', 'M3 21h8v-6H3z', 'M13 9h8V3h-8z'],
  tools: ['M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71', 'M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71'],
  reports: ['M4 20V10', 'M10 20V4', 'M16 20v-7', 'M22 20H2'],
  health: ['M3 12h4l3-8 4 16 3-8h4'],
  history: ['M3 12a9 9 0 1 0 3-6.7', 'M3 4v5h5', 'M12 7v5l3 2'],
  integrations: ['M8 3v4', 'M16 3v4', 'M6 7h12v4a6 6 0 0 1-12 0z', 'M12 17v4'],
  jobs: ['M4 6h16', 'M4 12h10', 'M4 18h7', 'M18 15l3 3-3 3'],
  sites: ['M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z', 'M3 12h18', 'M12 3a14 14 0 0 1 0 18', 'M12 3a14 14 0 0 0 0 18'],
  alljobs: ['M4 5h16v4H4z', 'M4 13h16v6H4z', 'M8 16h3'],
  system: ['M4 4h16v12H4z', 'M8 20h8', 'M12 16v4', 'M7 12l3-3 2 2 4-4'],
  downloads: ['M12 3v12', 'M7 10l5 5 5-5', 'M4 19h16'],
  docs: ['M5 4h10l4 4v12H5z', 'M15 4v4h4', 'M8 12h8', 'M8 16h6'],
  settings: ['M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z', 'M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z'],
  cpu: ['M7 7h10v10H7z', 'M10 10h4v4h-4z', 'M9 2v3', 'M15 2v3', 'M9 19v3', 'M15 19v3', 'M2 9h3', 'M2 15h3', 'M19 9h3', 'M19 15h3'],
  key: ['M15 7a4 4 0 1 1-3.9 4.9L3 20v-3h3v-3h3l2.1-2.1', 'M16 7h.01'],
  logs: ['M4 4h16v16H4z', 'M8 8h8', 'M8 12h8', 'M8 16h5'],
  menu: ['M4 6h16', 'M4 12h16', 'M4 18h16'],
  close: ['M6 6l12 12', 'M18 6L6 18'],
  help: ['M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z', 'M9.1 9a3 3 0 0 1 5.8 1c0 2-3 3-3 3', 'M12 17h.01'],
  chevron: ['M8 10l4 4 4-4'],
  bulb: ['M9 18h6', 'M10 21h4', 'M12 3a6 6 0 0 0-4 10.5c.7.7 1 1.5 1 2.5h6c0-1 .3-1.8 1-2.5A6 6 0 0 0 12 3z'],
  linkrun: ['M5 4a2 2 0 1 1 0 4a2 2 0 1 1 0-4z', 'M19 16a2 2 0 1 1 0 4a2 2 0 1 1 0-4z', 'M7 6h4a3 3 0 0 1 3 3v6a3 3 0 0 0 3 3', 'M17 3l2 2-2 2'],
  external: ['M14 4h6v6', 'M20 4l-9 9', 'M18 14v6H4V6h6'],
  team: ['M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8z', 'M2 21v-1a6 6 0 0 1 12 0v1', 'M16 3.1a4 4 0 0 1 0 7.8', 'M22 21v-1a6 6 0 0 0-4-5.6'],
  shield: ['M12 3l8 3v6c0 4.5-3.4 8.3-8 9-4.6-.7-8-4.5-8-9V6z', 'M9 12l2 2 4-4'],
  user: ['M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8z', 'M4 21v-1a8 8 0 0 1 16 0v1'],
  building: ['M4 21V5l8-3 8 3v16', 'M2 21h20', 'M9 9h1', 'M14 9h1', 'M9 13h1', 'M14 13h1', 'M10 21v-4h4v4'],
};

export type IconName = keyof typeof PATHS;

export function Icon({ name, size = 16, className }: { name: IconName; size?: number; className?: string }) {
  const d = PATHS[name] ?? [];
  return (
    <svg
      className={className ? `icon ${className}` : 'icon'}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {d.map((p) => (
        <path key={p} d={p} />
      ))}
    </svg>
  );
}
