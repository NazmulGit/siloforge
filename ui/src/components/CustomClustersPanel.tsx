// Custom Silo Network panel (Gap B): list clusters, export to JSON, import from a file.
import { useCallback, useEffect, useRef, useState } from 'react';
import { exportClusters, fetchClusters, importClusters } from '../api';
import { useToast } from '../toast';
import type { ClusterExport, ClusterInfo } from '../types';

export function CustomClustersPanel({ siteId }: { siteId: string }) {
  const toast = useToast();
  const [clusters, setClusters] = useState<ClusterInfo[] | null>(null);
  const [busy, setBusy] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);

  const load = useCallback(() => {
    fetchClusters(siteId)
      .then(setClusters)
      .catch((e: unknown) => toast.push(e instanceof Error ? e.message : 'Failed to load clusters', 'error'));
  }, [siteId, toast]);

  useEffect(() => {
    setClusters(null);
    load();
  }, [load]);

  async function doExport() {
    setBusy(true);
    try {
      const data = await exportClusters(siteId);
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `siloforge-clusters-${siteId}.json`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
      toast.push(`Exported ${data.clusters.length} cluster${data.clusters.length === 1 ? '' : 's'}`, 'success');
    } catch (e) {
      toast.push(e instanceof Error ? e.message : 'Export failed', 'error');
    } finally {
      setBusy(false);
    }
  }

  async function onFile(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    if (!file) return;
    setBusy(true);
    try {
      const text = await file.text();
      const parsed = JSON.parse(text) as ClusterExport;
      if (!parsed || !Array.isArray(parsed.clusters)) {
        throw new Error('Invalid file: expected { clusters: [...] }');
      }
      const res = await importClusters(siteId, parsed);
      toast.push(
        `Imported ${res.clusters} cluster${res.clusters === 1 ? '' : 's'} · ${res.members} posts assigned · ${res.skipped} skipped`,
        'success',
      );
      load();
    } catch (err) {
      toast.push(err instanceof Error ? err.message : 'Import failed', 'error');
    } finally {
      setBusy(false);
      if (fileRef.current) fileRef.current.value = '';
    }
  }

  return (
    <div className="card" style={{ marginBottom: 14 }}>
      <div className="card-title">Custom clusters</div>
      <div className="table-toolbar" style={{ marginBottom: 10 }}>
        <span className="muted">
          {clusters === null ? 'Loading…' : `${clusters.length} cluster${clusters.length === 1 ? '' : 's'}`}
        </span>
        <div className="spacer" />
        <button
          className="btn btn-sm"
          disabled={busy}
          onClick={() => void doExport()}
          data-hint="Download this site's silo/cluster network as a JSON file you can edit offline and re-import."
        >
          Export JSON
        </button>
        <button
          className="btn btn-sm btn-primary"
          disabled={busy}
          onClick={() => fileRef.current?.click()}
          data-hint="Upload a cluster JSON file to assign posts to custom clusters. Imported clusters are marked 'custom' and survive re-plans."
        >
          Import JSON
        </button>
        <input ref={fileRef} type="file" accept="application/json,.json" style={{ display: 'none' }} onChange={(e) => void onFile(e)} />
      </div>
      {clusters && clusters.length > 0 && (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>Label</th>
                <th>Source</th>
                <th className="num">Members</th>
                <th>Pillar</th>
              </tr>
            </thead>
            <tbody>
              {clusters.map((c) => (
                <tr key={c.id}>
                  <td>{c.label || '(unnamed)'}</td>
                  <td>
                    <span className={`chip chip-${c.source === 'custom' ? 'approved' : 'mode'}`}>{c.source}</span>
                  </td>
                  <td className="num">{c.members}</td>
                  <td
                    className="mono muted"
                    style={{ maxWidth: 260, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}
                    title={c.pillar_url ?? ''}
                  >
                    {c.pillar_url ?? '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <div className="faint" style={{ marginTop: 8, fontSize: 12.5 }}>
        Export a network to JSON, edit membership offline, and re-import — imported clusters are marked
        <span className="chip chip-approved" style={{ margin: '0 4px' }}>custom</span> and survive re-plans.
      </div>
    </div>
  );
}
