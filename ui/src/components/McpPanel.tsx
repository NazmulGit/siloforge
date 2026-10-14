import { useEffect, useMemo, useState } from 'react';
import { Link } from 'react-router-dom';
import { fetchMcpInfo, isUnavailable, v2ErrorMessage } from '../api2';
import { useToast } from '../toast';
import type { McpInfo } from '../types2';
import { Skeleton } from './Skeleton';
import { FeatureUnavailable } from './V2Notice';

/** Quote a YAML scalar only when needed (Windows paths go single-quoted). */
function yamlStr(s: string): string {
  return /^[A-Za-z0-9_./-]+$/.test(s) ? s : `'${s.replace(/'/g, "''")}'`;
}

function CopyBlock({ text, label }: { text: string; label: string }) {
  const toast = useToast();
  return (
    <div className="code-block">
      <pre>{text}</pre>
      <button
        className="btn btn-sm code-copy"
        aria-label={`Copy ${label}`}
        onClick={() => {
          void navigator.clipboard.writeText(text).then(
            () => toast.push(`${label} copied`, 'success'),
            () => toast.push('Copy failed — select the text manually', 'error'),
          );
        }}
      >
        Copy
      </button>
    </div>
  );
}

/** Settings → Integrations: how to connect Hermes Agent / MCP clients to this server. */
export function McpPanel() {
  const [info, setInfo] = useState<McpInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);

  useEffect(() => {
    let alive = true;
    fetchMcpInfo()
      .then((i) => {
        if (alive) setInfo(i);
      })
      .catch((e: unknown) => {
        if (!alive) return;
        if (isUnavailable(e)) setUnavailable(true);
        else setError(v2ErrorMessage(e, 'Failed to load MCP info'));
      });
    return () => {
      alive = false;
    };
  }, []);

  const apiUrl = info?.api_url || window.location.origin;
  const args = useMemo(() => (info?.stdio_args && info.stdio_args.length > 0 ? info.stdio_args : ['mcp']), [info]);

  const stdioLine = useMemo(
    () => (info ? [info.stdio_command, ...args].map((a) => (/\s/.test(a) ? `"${a}"` : a)).join(' ') : ''),
    [info, args],
  );

  const hermesYaml = useMemo(() => {
    if (!info) return '';
    return [
      'mcp_servers:',
      '  siloforge:',
      `    command: ${yamlStr(info.stdio_command)}`,
      `    args: [${args.map(yamlStr).join(', ')}]`,
      '    env:',
      `      SILOFORGE_API_URL: ${yamlStr(apiUrl)}`,
      '      SILOFORGE_TOKEN: sfk_paste_your_token_here',
    ].join('\n');
  }, [info, args, apiUrl]);

  if (unavailable) {
    return (
      <div className="card">
        <FeatureUnavailable feature="The MCP server" />
      </div>
    );
  }
  if (error) return <div className="error-box">{error}</div>;
  if (!info) return <Skeleton rows={4} />;

  return (
    <>
      <div className="card">
        <div className="card-title">Connection</div>
        {info.enabled === false && (
          <div className="warn-note" style={{ marginBottom: 10 }}>
            The MCP server is switched off in this server&apos;s config.
          </div>
        )}
        <div className="settings-row-label">stdio (local agents)</div>
        <div className="settings-row-desc" style={{ marginBottom: 6 }}>
          The agent starts SiloForge itself and talks over stdin/stdout.
        </div>
        <CopyBlock text={stdioLine} label="stdio command" />
        <div className="settings-row-label" style={{ marginTop: 14 }}>
          Streamable HTTP (remote agents)
        </div>
        <div className="settings-row-desc" style={{ marginBottom: 6 }}>
          Send an API token as <code>Authorization: Bearer sfk_…</code>.
        </div>
        <CopyBlock text={info.http_url} label="HTTP URL" />
      </div>

      <div className="card">
        <div className="card-title">Hermes Agent</div>
        <p className="muted" style={{ marginTop: 0, fontSize: 13 }}>
          Paste this into the profile&apos;s <code>config.yaml</code>, then replace the token with one from{' '}
          <Link to="/settings?section=tokens">API tokens</Link> (scopes <code>read</code> + <code>write</code> are enough
          for linking work; <code>siloforge_list_notifications</code>, <code>siloforge_create_backup</code> and the full{' '}
          <code>siloforge_system_status</code> need an admin user&apos;s token with the <code>admin</code> scope). Restart
          the Hermes gateway afterwards.
        </p>
        <CopyBlock text={hermesYaml} label="Hermes config" />
      </div>

      {info.tools && info.tools.length > 0 && (
        <div className="card">
          <div className="card-title">Tools ({info.tools.length})</div>
          <div className="tbl-wrap">
            <table className="tbl">
              <tbody>
                {info.tools.map((t) => (
                  <tr key={t.name}>
                    <td className="mono" style={{ whiteSpace: 'nowrap' }}>
                      {t.name}
                    </td>
                    <td className="muted">{t.description ?? ''}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </>
  );
}
