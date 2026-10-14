// Knowledgebase — Standard Operating Procedures for operating SiloForge.
// Pure static documentation: no props, no app context, no API calls.
// Rendered inside Settings → Knowledge Base. Styling relies on existing
// theme tokens (var(--panel), var(--accent) …) via inline styles plus a few
// shared classes (.card, .card-title, .page-sub, .mono-sm) so it needs no
// edits to styles.css. Collapsible <details> need no JavaScript.
import type { CSSProperties, ReactNode } from 'react';

interface Step {
  /** The literal UI path for this step, e.g. "Dashboard → Run Plan". */
  path?: string;
  text: ReactNode;
}

interface Sop {
  id: string;
  title: string;
  /** One- or two-line "what / why". */
  what: string;
  steps: Step[];
  note?: ReactNode;
}

// ---- SOP content (serial operating lifecycle) ----

const SOPS: Sop[] = [
  {
    id: 'add-site',
    title: 'Add a site',
    what: 'Register a site so SiloForge can crawl it. Adding a site mints the API key your WordPress plugin needs.',
    steps: [
      { path: 'Sites → + Add Site', text: 'Open the Sites page and click + Add Site.' },
      { text: 'Enter the Domain (e.g. example.com) and, optionally, the WordPress URL.' },
      {
        text: 'Choose the Crawl mode — wp_rest (WordPress REST API) or sitemap (XML sitemap crawl) — then click Create site.',
      },
      {
        text: 'A modal shows the generated API key (sf_…). Copy it now with the Copy button — it is shown only once.',
      },
    ],
    note: 'The key is your plugin credential. Store it before closing the modal; if you lose it you must re-open the site to read it again.',
  },
  {
    id: 'connect-wp',
    title: 'Connect WordPress',
    what: 'Install the SiloForge Connect plugin and pair it with the site so content can sync up and approved links can pull down.',
    steps: [
      {
        text: 'Download the plugin zip from the connect repo’s releases/ folder (releases/siloforge-connect-<version>.zip).',
      },
      {
        path: 'WP Admin → Plugins → Add New → Upload Plugin',
        text: 'Upload the zip and Activate.',
      },
      {
        path: 'Settings → SiloForge',
        text: 'Set the API URL (your SiloForge backend, default http://<host>:8600) and paste the API Key from SOP 1.',
      },
      { text: 'Click Test Connection to confirm the handshake succeeds (the page shows “Connected”).' },
      { text: 'Click Sync Now to push post and page content up for analysis.' },
    ],
    note: 'Non-WordPress or sitemap-mode sites skip the plugin — ingest with Crawl (SOP 3) and, for injection, use CSV (SOP 14).',
  },
  {
    id: 'crawl',
    title: 'Crawl the site',
    what: 'Pull the site’s posts and pages into the content index. This reads content only — no links are changed.',
    steps: [
      {
        path: 'Dashboard → Run Crawl',
        text: 'Select the site and click Run Crawl (or use Sites → the site’s row → Crawl).',
      },
      { text: 'Track progress on the Recent jobs card or the Jobs page.' },
      { text: 'When it finishes, the Posts count on the Dashboard reflects the freshly indexed content.' },
    ],
    note: 'Push alternative: the plugin’s Sync Now uploads content directly, so a crawl job is not always required.',
  },
  {
    id: 'run-plan',
    title: 'Run a plan (topical match)',
    what: 'The core NLP step: turn indexed content into scored link suggestions. Nothing is written to the site.',
    steps: [
      {
        path: 'Dashboard → topology selector',
        text: 'Choose a silo topology — reverse (support → pillar), serial (chain), priority (equity to top pages) or circle (loop).',
      },
      { path: 'Dashboard → Run Plan', text: 'Click Run Plan.' },
      {
        text: 'The plan embeds content (local all-MiniLM SBERT), builds the graph (PageRank, orphans, dead-ends, click-depth), clusters posts into topical silos, scores every candidate on the 11-dimension relevancy model, and generates anchors.',
      },
      { text: 'Suggested edges then appear across the Tools tabs and in the Dashboard counters.' },
    ],
    note: 'Can take minutes on large sites — embedding is the bottleneck, and the very first run also downloads the model.',
  },
  {
    id: 'review-dashboard',
    title: 'Review the dashboard',
    what: 'Read the health signals and know what to do about each one.',
    steps: [
      { path: 'Dashboard', text: 'Link Health Score (0–100): overall internal-linking health. Higher is better.' },
      {
        text: 'Orphans — pages with no inbound links, effectively unreachable. Fix via Quick Fix Orphans or Tools → Inbound.',
      },
      {
        text: 'Dead ends — pages with no outbound links, passing no equity onward. Fix via Quick Fix Deadends or Tools → Outbound.',
      },
      {
        text: 'Opportunities — count of suggested links awaiting review. Click the card to open Tools → Inbound.',
      },
      { text: 'HTTP Links — internal links still on insecure http:// instead of https://.' },
      { text: 'Outbound Domains — distinct external domains your content links out to.' },
      {
        text: 'Link Duplication — repeated links to the same target from the same page; usually worth trimming.',
      },
      {
        text: 'Cannibalization warnings — several pages competing for the same term/anchor; consolidate or re-target.',
      },
    ],
    note: 'The Quick Fix buttons generate suggestions (running a Plan first if none exist) and then drop you straight into the approval queue.',
  },
  {
    id: 'bulk-wizard',
    title: 'Use the Bulk wizard',
    what: 'The guided flow that actually injects links, from choosing source posts to writing them live.',
    steps: [
      { path: 'Tools → Bulk → + New Bulk', text: 'Open the guided wizard.' },
      {
        path: 'Step 1 · Fetch',
        text: 'Pick the posts to link FROM — scope by All posts, topical cluster, post type or search; tick “Orphans only” if wanted; or Import a URL list. Select sources and click Submit / Start matching.',
      },
      {
        path: 'Step 2 · Match',
        text: 'Auto-match by topical map (semantic embeddings + clusters, 11-dimension relevancy). Click Start matching, or View results if the posts are already scored.',
      },
      { path: 'Step 3 · Progress', text: 'Watch the topical match run.' },
      {
        path: 'Step 4 · Results',
        text: 'Review each source → target with its relevancy score. Filter with the Min relevancy slider, then Approve individual rows, Approve selected, or Approve all ≥ threshold. Export CSV/JSON is optional.',
      },
      { path: 'Step 5 · Link', text: 'Click Start Linking to inject the approved links.' },
    ],
    note: 'Set Mode and Preset in Tools → Bulk → Insertion settings. Mode = Pipeline (native → rephrase → context) or a Single module. Preset = conservative / balanced / aggressive. The three insertion modules: Native ≥60 wraps an existing sentence, Rephrase ≥40 rewrites one sentence, Context ≥30 adds 2–3 sentences.',
  },
  {
    id: 'tool-tabs',
    title: 'Inbound / Outbound / Silo / Archive Silo tools',
    what: 'The per-strategy candidate queues where you approve or reject individual suggestions.',
    steps: [
      {
        path: 'Tools → Inbound',
        text: 'Candidates grouped by the page they point TO — use it to give orphan or weak pages incoming links.',
      },
      {
        path: 'Tools → Outbound',
        text: 'Candidates grouped by the page they come FROM — use it to give dead-end pages outgoing links.',
      },
      {
        path: 'Tools → Silo',
        text: 'Suggestions that follow your silo topology, keeping link equity inside topical clusters.',
      },
      {
        path: 'Tools → Archive Silo → Build Archive Silo',
        text: 'Generate links between posts and their WordPress category / tag archive pages.',
      },
      {
        text: 'Approve or Reject each row (or bulk-select several). Click a score to open the 11-dimension breakdown popover.',
      },
    ],
    note: 'Approving only marks an edge injectable — nothing is written to the site until you inject (SOP 10).',
  },
  {
    id: 'anchors',
    title: 'Anchor Manager',
    what: 'Review and balance anchor text so the link profile does not look over-optimized.',
    steps: [
      {
        path: 'Tools → Anchors',
        text: 'See the anchor-type distribution — exact / partial / branded / generic / LSI.',
      },
      { text: 'Inline-edit any anchor’s text or type (editing is blocked once an edge has been applied).' },
      {
        text: 'Heed the over-optimization guard: it warns when exact-match anchors exceed 40% or generic exceed 30%.',
      },
    ],
    note: 'The guard also runs automatically during a Plan, rebalancing or flagging anchors before they become suggestions.',
  },
  {
    id: 'rules',
    title: 'Rules: Keyword Linker & Ignore Keywords',
    what: 'Deterministic keyword → URL linking, plus a stop-list of phrases the linker must never touch.',
    steps: [
      {
        path: 'Tools → Keyword Linker',
        text: 'Add a rule (keyword, target URL, anchor type, max links) and Run it to create keyword-origin edges wherever that phrase appears.',
      },
      {
        path: 'Tools → Ignore Keywords',
        text: 'Add phrases the linker must never turn into anchor text (brand safety). These filter both anchor generation and the keyword linker.',
      },
    ],
    note: 'Keyword-linked edges still pass through the same approval and injection gate as every other suggestion.',
  },
  {
    id: 'inject',
    title: 'Approve & inject',
    what: 'The hard gate: only approved edges are ever written, and every write is snapshotted first.',
    steps: [
      { text: 'Approve suggestions in any tool tab or the Bulk wizard (SOP 6–9).' },
      {
        path: 'Tools → Bulk → Run Bulk Inject',
        text: 'Inject every approved edge into your live content.',
      },
      {
        text: 'Injection selects a module per edge (Native / Rephrase / Context per your Mode) and writes via WP REST or a plugin pull. Plugin path: click Pull & Apply Links in WP Admin → Settings → SiloForge.',
      },
    ],
    note: 'A per-post snapshot is taken before every write. Nothing injects unless its status is approved. No tracking parameters are ever added to your links.',
  },
  {
    id: 'verify-undo',
    title: 'Verify & undo',
    what: 'Confirm the links landed, and roll back safely if anything looks wrong.',
    steps: [
      {
        path: 'History',
        text: 'Every applied and reverted link — source → target, anchor, tool, status, applied time. Download CSV for an audit trail.',
      },
      { text: 'Spot-check a few live pages: the anchor <a> should be present in the published HTML.' },
      {
        path: 'Tools → Bulk → Undo All',
        text: 'Revert applied links; snapshots restore the exact pre-inject content. Selective / bulk undo is also available.',
      },
    ],
    note: 'Reverted edges are re-approvable — you can undo, adjust, and re-inject.',
  },
  {
    id: 'reports',
    title: 'Reports & Visualizer',
    what: 'Per-post link metrics and a visual map of the whole link graph.',
    steps: [
      {
        path: 'Reports → Links',
        text: 'Sortable per-post table — inbound, outbound, PageRank, click depth, Link Flow score and duplicate count. Narrow it with the Min/Max column filters.',
      },
      {
        text: 'From the Links table, tick posts and Send selection to Bulk, or Export filtered to Bulk — this approves their suggested edges and routes them to Tools → Bulk.',
      },
      {
        path: 'Reports → Visualizer',
        text: 'Interactive graph — nodes are posts (colour = cluster, size = PageRank), orphans are ringed red, edges are styled by status.',
      },
      { text: 'The anchor-type distribution report lives in Tools → Anchors.' },
    ],
  },
  {
    id: 'custom-silo',
    title: 'Custom Silo Network',
    what: 'Export or import your topical clusters as JSON so you can hand-craft the network.',
    steps: [
      {
        path: 'Tools → Silo → Custom clusters',
        text: 'Export JSON to download the current clusters ({ label, pillar_url, member_urls }).',
      },
      {
        text: 'Edit the JSON and Import it. Import replaces your custom clusters; posts are matched by exact URL, and unknown URLs are skipped.',
      },
    ],
    note: 'Imported custom clusters survive re-plans — re-clustering only rebuilds the auto clusters and leaves pinned custom ones alone.',
  },
  {
    id: 'csv',
    title: 'CMS / CSV for non-WordPress',
    what: 'Feed non-WordPress content in and take decided links out, entirely as CSV.',
    steps: [
      {
        path: 'CMS Integration → CSV Import',
        text: 'Upload a CSV with the header url,title,content_html (from Webflow, Ghost or any CMS). Rows upsert into the same index as the WordPress sync (deduped by content hash). Then run a Plan.',
      },
      {
        path: 'CMS Integration → CSV Export',
        text: 'Download approved & applied links (source_url,target_url,anchor,anchor_type,status,insert_module) to place manually on any platform.',
      },
    ],
  },
  {
    id: 'gsc',
    title: 'Search Console (GSC)',
    what: 'Optional Google Search Console signals that sharpen link scoring.',
    steps: [
      {
        text: 'Provide Google Search Console credentials to the backend (GSC_CREDENTIALS_JSON). This is a backend/env integration configured by the operator, not an in-app button.',
      },
      {
        text: 'On the next Plan, GSC supplies query volume and average position; pages ranking in positions 4–10 are flagged striking-distance.',
      },
      {
        text: 'These signals feed the gsc scoring dimension and nudge links toward striking-distance and money pages.',
      },
    ],
    note: 'Entirely optional — with no credentials the GSC dimension simply contributes 0, and every other step still works.',
  },
  {
    id: 'accounts',
    title: 'Multi-user & accounts',
    what: 'Each user owns their own sites; sign in to see and manage only yours.',
    steps: [
      {
        path: 'Register',
        text: 'Create an account (name, email, password ≥ 8 chars). You are logged in automatically.',
      },
      {
        path: 'Login',
        text: 'On return, sign in with email + password; your session token is stored in this browser.',
      },
      {
        path: 'Settings → Account',
        text: 'See the email you are signed in as and your role chip — admin sees every site, user sees only their own.',
      },
    ],
    note: 'Sites you add are scoped to you — another user’s site is invisible to you (it returns 404 rather than revealing it). Admins bypass scoping.',
  },
  {
    id: 'safety',
    title: 'Safety & best practice',
    what: 'A safe operating order that avoids surprises on a live site.',
    steps: [
      { text: 'Back up the site (the tool also takes an automatic per-post snapshot before every write).' },
      { text: 'Test on a few posts first — scope a small Bulk run and review the live result.' },
      { text: 'Once you are happy, run in bulk.' },
    ],
    note: 'Defaults are conservative: native-only insertion, nothing injects without approval, and no tracking parameters are ever added to your links.',
  },
  {
    id: 'troubleshooting',
    title: 'Troubleshooting',
    what: 'Common issues and their fixes.',
    steps: [
      {
        text: 'Plan is slow — embedding is the bottleneck on big sites, and the first run also downloads the model. Let it finish; later plans are faster.',
      },
      {
        text: 'Crawl blocked / cannot reach a local host — the SSRF guard blocks private hosts. For a local or dev target, add the host to CRAWL_ALLOW_PRIVATE_HOSTS (e.g. site01.example,localhost,127.0.0.1).',
      },
      {
        text: 'Sync returns 413 — resolved by byte-batching; the plugin splits content into smaller batches. Update the plugin if you still see it.',
      },
      {
        text: 'Empty results when fetching an archive / tag cluster — archive and tag pages are link targets, not sources. Fetch real posts as sources instead.',
      },
    ],
  },
];

// ---- styles (inline, reading existing theme tokens) ----

const wrap: CSSProperties = { maxWidth: 900 };

const introCard: CSSProperties = { marginBottom: 16 };

const tocGrid: CSSProperties = {
  display: 'grid',
  gridTemplateColumns: 'repeat(auto-fill, minmax(240px, 1fr))',
  gap: '6px 18px',
};

const tocLink: CSSProperties = {
  display: 'flex',
  gap: 10,
  alignItems: 'baseline',
  padding: '4px 0',
  color: 'var(--text)',
};

const tocNum: CSSProperties = {
  flex: 'none',
  width: 22,
  textAlign: 'right',
  color: 'var(--accent)',
  fontWeight: 700,
  fontVariantNumeric: 'tabular-nums',
};

const sopCard: CSSProperties = {
  background: 'var(--panel)',
  border: '1px solid var(--border)',
  borderRadius: 'var(--radius)',
  boxShadow: 'var(--shadow-sm)',
  marginBottom: 10,
};

const summary: CSSProperties = {
  display: 'flex',
  alignItems: 'baseline',
  gap: 12,
  padding: '14px 18px',
  cursor: 'pointer',
  userSelect: 'none',
  fontWeight: 600,
  fontSize: 15,
  color: 'var(--text)',
};

const summaryNum: CSSProperties = {
  flex: 'none',
  minWidth: 26,
  height: 26,
  display: 'inline-flex',
  alignItems: 'center',
  justifyContent: 'center',
  borderRadius: 999,
  background: 'var(--accent-dim)',
  color: 'var(--accent)',
  fontSize: 13,
  fontWeight: 700,
  fontVariantNumeric: 'tabular-nums',
  alignSelf: 'center',
};

const bodyStyle: CSSProperties = {
  padding: '0 18px 18px 56px',
  display: 'flex',
  flexDirection: 'column',
  gap: 12,
};

const whatStyle: CSSProperties = { color: 'var(--muted)', fontSize: 13.5, margin: 0 };

const olStyle: CSSProperties = {
  margin: 0,
  paddingLeft: 20,
  display: 'flex',
  flexDirection: 'column',
  gap: 8,
};

const liStyle: CSSProperties = { fontSize: 13.5, lineHeight: 1.5 };

const pathPill: CSSProperties = {
  display: 'inline-block',
  fontFamily: 'var(--mono)',
  fontSize: 12,
  fontWeight: 600,
  color: 'var(--accent)',
  background: 'var(--accent-dim)',
  border: '1px solid rgba(47, 107, 255, 0.28)',
  borderRadius: 6,
  padding: '1px 7px',
  marginRight: 8,
  whiteSpace: 'nowrap',
};

const noteStyle: CSSProperties = {
  display: 'flex',
  gap: 10,
  alignItems: 'baseline',
  background: 'var(--accent-dim)',
  border: '1px solid rgba(47, 107, 255, 0.28)',
  borderRadius: 'var(--radius)',
  padding: '10px 14px',
  fontSize: 13,
  color: 'var(--text)',
};

const noteBadge: CSSProperties = {
  flex: 'none',
  fontSize: 11,
  fontWeight: 700,
  letterSpacing: '0.06em',
  textTransform: 'uppercase',
  color: 'var(--accent)',
};

// ---- component ----

export function Knowledgebase() {
  return (
    <div style={wrap}>
      <div className="page-head" style={{ marginBottom: 16 }}>
        <div>
          <h1>Knowledge Base</h1>
          <div className="page-sub">
            How to operate SiloForge — follow these standard operating procedures in order.
          </div>
        </div>
      </div>

      <div className="card" style={introCard}>
        <p style={{ margin: 0, color: 'var(--muted)', fontSize: 13.5, lineHeight: 1.55 }}>
          SiloForge is a self-hosted internal-linking platform. The natural operating lifecycle runs
          top to bottom: connect a site, index its content, plan links with NLP, review, then approve
          and inject — with a snapshot before every write and an undo for everything. Each procedure
          below names the exact UI path to click. Expand any step to read it.
        </p>
      </div>

      <div className="card" style={{ marginBottom: 20 }}>
        <div className="card-title">Contents</div>
        <div style={tocGrid}>
          {SOPS.map((sop, i) => (
            <a key={sop.id} href={`#kb-${sop.id}`} style={tocLink}>
              <span style={tocNum}>{i + 1}</span>
              <span>{sop.title}</span>
            </a>
          ))}
        </div>
      </div>

      {SOPS.map((sop, i) => (
        <details key={sop.id} id={`kb-${sop.id}`} style={sopCard} open={i === 0}>
          <summary style={summary}>
            <span style={summaryNum}>{i + 1}</span>
            <span>{sop.title}</span>
          </summary>
          <div style={bodyStyle}>
            <p style={whatStyle}>{sop.what}</p>
            <ol style={olStyle}>
              {sop.steps.map((step, j) => (
                <li key={j} style={liStyle}>
                  {step.path && <code style={pathPill}>{step.path}</code>}
                  <span>{step.text}</span>
                </li>
              ))}
            </ol>
            {sop.note && (
              <div style={noteStyle} role="note">
                <span style={noteBadge}>Note</span>
                <span>{sop.note}</span>
              </div>
            )}
          </div>
        </details>
      ))}
    </div>
  );
}
