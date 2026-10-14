import { useEffect, useMemo, useState } from 'react';
import { downloadRelease, fetchDownloads, isUnavailable, v2ErrorMessage } from '../api2';
import { CopyButton } from '../components/CopyButton';
import { Skeleton } from '../components/Skeleton';
import { FeatureUnavailable } from '../components/V2Notice';
import { HelpButton } from '../docs';
import { fmtBytes, fmtDate } from '../format';
import { useToast } from '../toast';
import type { DownloadItem, DownloadsList } from '../types2';

const KINDS: Array<{ kind: string; title: string; blurb: string; steps: string[] }> = [
  {
    kind: 'app',
    title: 'SiloForge app',
    blurb: 'The single Windows binary: API, web UI, MCP server, backups and the CLI.',
    steps: [
      'Take a backup first (Settings, Backups).',
      'Stop the running service, then unzip over the install folder (keep the data folder).',
      'Start SiloForge again and check the version in the sidebar.',
    ],
  },
  {
    kind: 'wp-plugin',
    title: 'WordPress plugin (SiloForge Connect)',
    blurb: 'Installs on each WordPress site so SiloForge can read posts and place links.',
    steps: [
      'In WordPress, open Plugins, Add New, Upload Plugin and choose this zip.',
      'Activate it, then paste the site API key from the Sites page into its settings.',
      'Run a crawl from the site Dashboard to confirm the connection.',
    ],
  },
];

/** Version-aware sort, newest first ("2.10.0" before "2.9.1"). */
function cmpVersion(a: string, b: string): number {
  const pa = a.split(/[.-]/).map((x) => Number.parseInt(x, 10) || 0);
  const pb = b.split(/[.-]/).map((x) => Number.parseInt(x, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pb[i] ?? 0) - (pa[i] ?? 0);
    if (d) return d;
  }
  return 0;
}

function kindLabel(kind: string): string {
  if (kind === 'app') return 'App';
  if (kind === 'wp-plugin') return 'WP plugin';
  return kind;
}

function Sha({ value }: { value: string }) {
  if (!value) return <span className="faint">no checksum</span>;
  return (
    <span className="sha">
      <code className="mono-sm" title={value}>
        {value.slice(0, 12)}…{value.slice(-6)}
      </code>
      <CopyButton text={value} what="SHA-256" />
    </span>
  );
}

export function DownloadsPage() {
  const toast = useToast();
  const [data, setData] = useState<DownloadsList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    fetchDownloads()
      .then((d) => {
        if (alive) setData(d);
      })
      .catch((e: unknown) => {
        if (!alive) return;
        if (isUnavailable(e)) setUnavailable(true);
        else setError(v2ErrorMessage(e, 'Failed to load downloads'));
      });
    return () => {
      alive = false;
    };
  }, []);

  async function download(file: string) {
    setBusy(file);
    try {
      await downloadRelease(file);
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Download failed'), 'error');
    } finally {
      setBusy(null);
    }
  }

  const byKind = useMemo(() => {
    const m = new Map<string, DownloadItem[]>();
    for (const d of data?.downloads ?? []) m.set(d.kind, [...(m.get(d.kind) ?? []), d]);
    for (const [k, list] of m) m.set(k, [...list].sort((a, b) => cmpVersion(a.version, b.version)));
    return m;
  }, [data]);

  const allRows = useMemo(() => {
    const known = KINDS.flatMap((k) => byKind.get(k.kind) ?? []);
    const other = [...byKind.entries()].filter(([kind]) => !KINDS.some((k) => k.kind === kind)).flatMap(([, l]) => l);
    return [...known, ...other];
  }, [byKind]);

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>Downloads</h1>
          <div className="page-sub">
            Release packages served by this server{data?.current_version ? `. This server runs v${data.current_version}.` : '.'}
          </div>
        </div>
        <div className="spacer" />
        <HelpButton />
      </div>
      {unavailable ? (
        <div className="card">
          <FeatureUnavailable feature="Downloads" />
        </div>
      ) : error ? (
        <div className="error-box">{error}</div>
      ) : !data ? (
        <Skeleton rows={6} />
      ) : (
        <>
          <div className="download-grid">
            {KINDS.map((k) => {
              const list = byKind.get(k.kind) ?? [];
              const latest = list.find((d) => d.latest) ?? list[0];
              return (
                <section key={k.kind} className="card download-card">
                  <div className="download-head">
                    <h2>{k.title}</h2>
                    {latest && <span className="version-badge">v{latest.version}</span>}
                  </div>
                  <p className="muted download-blurb">{k.blurb}</p>
                  {latest ? (
                    <>
                      <dl className="kv">
                        <dt>File</dt>
                        <dd className="mono-sm">{latest.file}</dd>
                        <dt>Size</dt>
                        <dd>{fmtBytes(latest.size_bytes)}</dd>
                        <dt>Built</dt>
                        <dd>{fmtDate(latest.created_at)}</dd>
                        <dt>SHA-256</dt>
                        <dd>
                          <Sha value={latest.sha256} />
                        </dd>
                      </dl>
                      <div className="form-actions">
                        <button className="btn btn-primary" disabled={busy === latest.file} onClick={() => void download(latest.file)}>
                          {busy === latest.file ? 'Downloading…' : `Download v${latest.version}`}
                        </button>
                      </div>
                    </>
                  ) : (
                    <div className="muted">No package on this server yet. Packages appear here after a release is built.</div>
                  )}
                  <details className="install-notes">
                    <summary>How to install</summary>
                    <ol className="steps">
                      {k.steps.map((s) => (
                        <li key={s}>{s}</li>
                      ))}
                    </ol>
                  </details>
                </section>
              );
            })}
          </div>

          {allRows.length > 0 && (
            <div className="card">
              <div className="card-title">All versions</div>
              <div className="tbl-wrap">
                <table className="tbl">
                  <thead>
                    <tr>
                      <th>Package</th>
                      <th>Version</th>
                      <th className="num">Size</th>
                      <th>Built</th>
                      <th>SHA-256</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {allRows.map((d) => (
                      <tr key={d.file}>
                        <td>{kindLabel(d.kind)}</td>
                        <td>
                          v{d.version} {d.latest && <span className="chip chip-done">latest</span>}
                        </td>
                        <td className="num">{fmtBytes(d.size_bytes)}</td>
                        <td className="muted">{fmtDate(d.created_at)}</td>
                        <td>
                          <Sha value={d.sha256} />
                        </td>
                        <td>
                          <button className="btn btn-sm" disabled={busy === d.file} onClick={() => void download(d.file)}>
                            {busy === d.file ? 'Downloading…' : 'Download'}
                          </button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}
