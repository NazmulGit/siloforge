// CMS Integration (Gap K-CSV): CSV import for non-WordPress content + edge CSV export.
import { useRef, useState } from 'react';
import { useParams } from 'react-router-dom';
import { downloadEdgesCsv, importCsv } from '../api';
import { EmptyState } from '../components/Skeleton';
import { useToast } from '../toast';

export function IntegrationsPage() {
  const { siteId } = useParams<{ siteId: string }>();
  const toast = useToast();
  const fileRef = useRef<HTMLInputElement>(null);
  const [busy, setBusy] = useState(false);

  if (!siteId) return <EmptyState title="No site selected" hint="Pick a site from the selector above." />;

  async function onImport(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0];
    if (!file || !siteId) return;
    setBusy(true);
    try {
      const text = await file.text();
      const res = await importCsv(siteId, text);
      toast.push(`Imported ${res.upserted} · skipped ${res.skipped} (of ${res.received} rows)`, 'success');
    } catch (err) {
      toast.push(err instanceof Error ? err.message : 'CSV import failed', 'error');
    } finally {
      setBusy(false);
      if (fileRef.current) fileRef.current.value = '';
    }
  }

  async function onExport() {
    if (!siteId || busy) return;
    setBusy(true);
    try {
      const blob = await downloadEdgesCsv(siteId);
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `siloforge-edges-${siteId}.csv`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (err) {
      toast.push(err instanceof Error ? err.message : 'CSV export failed', 'error');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div>
      <div className="page-head">
        <div>
          <h1>CMS Integration</h1>
          <div className="page-sub">Feed non-WordPress content in, and take decided links out — as CSV.</div>
        </div>
      </div>

      <div className="dash-cols">
        <div className="card">
          <div className="card-title">CSV Import</div>
          <p className="muted" style={{ marginTop: 0 }}>
            Upload a CSV to feed content from Webflow, Ghost or any CMS. Header row required:
          </p>
          <div className="apikey-box">
            <code>url,title,content_html</code>
          </div>
          <p className="faint" style={{ fontSize: 12.5 }}>
            Rows upsert into the same content store as the WordPress sync (deduped by content hash). Then run a Plan.
          </p>
          <div className="table-toolbar">
            <div className="spacer" />
            <button
              className="btn btn-primary"
              disabled={busy}
              onClick={() => fileRef.current?.click()}
              data-hint="Upload a url,title,content_html CSV to feed non-WordPress content into the index. Rows upsert (deduped by content hash); then run a Plan."
            >
              {busy ? 'Working…' : 'Choose CSV…'}
            </button>
            <input ref={fileRef} type="file" accept="text/csv,.csv" style={{ display: 'none' }} onChange={(e) => void onImport(e)} />
          </div>
        </div>

        <div className="card">
          <div className="card-title">CSV Export</div>
          <p className="muted" style={{ marginTop: 0 }}>
            Download approved &amp; applied links as CSV so you can place them manually on any CMS.
          </p>
          <div className="apikey-box">
            <code>source_url,target_url,anchor,anchor_type,status,insert_module</code>
          </div>
          <div className="table-toolbar">
            <div className="spacer" />
            <button
              className="btn"
              disabled={busy}
              onClick={() => void onExport()}
              data-hint="Download all approved and applied links as a CSV so you can place them manually on any CMS."
            >
              Download edges CSV
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
