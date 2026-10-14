# 01 — SiloForge Engine Porting Spec (TypeScript → Rust)

Source audited (read-only): `D:\Code\SiloForge-Suite\app\packages\engine\src` (45 files), `packages\engine\test`,
`packages\rust-engine\src`, `packages\shared\src` (config/qdrant/types), `db\migrations\*.sql`,
`packages\worker\src`, `packages\api\src\routes\suggestions.ts`, and docs `ARCHITECTURE.md`, `TECH_BREAKDOWN.md`,
`PLAN_v1.2.0.md`, `PLAN_v1.4.0.md`, `releases\v1.3.0\CAPABILITY_AUDIT.md`.

All file:line references are relative to `app/packages/engine/src/` unless prefixed.

Legend: **[RUST-EXISTS]** = already implemented in `packages/rust-engine`; **[TS-ONLY]** = must be ported.

---

## 0. System overview

### 0.1 Pipeline (`pipeline/plan.ts::planSite`)

```
posts (crawled) ──► 1 chunkPost (per post)            [html.analyze RUST-EXISTS]
                ──► 2 embedSite (posts + paragraphs)  [remote/ONNX]
                ──► 2b reapSiteVectors (Qdrant GC)  2c checkParagraphIntegrity
                ──► 3 extractKeywords  4 pullGsc
                ──► 5 buildGraph                      [html.analyze + graph.metrics RUST-EXISTS]
                ──► 6 clusterSite                     [kmeans RUST-EXISTS]
                ──► 7 buildTopologyEdges (per cluster, density-capped)
                ──► 8 addSemanticCandidates (Qdrant top-5 > 0.5)
                ──► 8b buildArchiveSilo (optional)
                ──► 8c applyDensityCaps (global)
                ──► 9 scoreCandidates (11-dim composite)
                ──► 10 generateAnchors (NLP + guards + predicted module)
```
Progress callbacks: Chunking 5, Embedding 30, Keywords 45, Graph 60, Clustering 70, Topology 78, Scoring 92, Anchors 100.
Topology resolution: explicit arg → `settings.silo_topology` → `'reverse'`.
Returns `PlanStats {posts, paragraphs, embedded (= posts+paragraphs embedded), candidates, scored, archive_candidates}`.

Separately triggered jobs (worker): `crawl`, `plan`, `inject`, `undo`, `keyword_link`, `link_health`, `audit`
(`audit` = crawl → plan → captureMetrics). Worker: BullMQ queue `siloforge`, `concurrency: 2`, two replicas
(`worker/src/index.ts:90`), attempts crawl=2 else 1. **No per-site/per-post lock exists.**

### 0.2 What already exists in Rust (`packages/rust-engine`, protocol v1, NDJSON over stdio)

| op | file | does | used by |
|---|---|---|---|
| `health` | protocol.rs | version/ops list | client |
| `html.analyze` | html.rs | canonical paragraph split + SHA-1 + link extraction | `chunkPost`, `buildGraph` |
| `graph.metrics` | graph.rs | PageRank, WCC, multi-source BFS depth, degrees, orphan/dead-end | `buildGraph`, `getComponents` |
| `kmeans` | kmeans.rs | k-means++ (FNV-1a + mulberry32 seeding), Lloyd iterations | `clusterSite` |

Limits: request ≤ 32 MiB (`protocol.rs:80`), html ≤ 16 MiB, ≤100k paragraphs, ≤250k links, ≤100k nodes, ≤1M edges,
≤100k vectors, dim ≤4096, ≤10M values, seed ≤4096 bytes, ids 1..128 chars. Crate deps pinned: scraper 0.20.0, serde 1.0.210,
serde_json 1.0.128, sha1 0.10.6, url 2.5.2. Release profile: lto thin, panic=abort.

TS client (`rust/client.ts`): persistent child process, multiplexed by id `${pid}-${seq}`, timeout 30 s, max 64 in-flight,
response cap 64 MiB. Mode `auto` (default; `SILOFORGE_RUST_ENGINE=off` → `fallback`). Any `RustEngineUnavailableError`
(spawn fail, exit, timeout, bad JSON, version mismatch) **permanently** switches the process to the JS fallback
(`rust/fallback.ts`). `RustEngineOperationError` (INVALID_INPUT etc.) propagates as an error (no fallback).

Everything else (crawl, embed, keywords, GSC, scoring, anchors, NLP, topology, density, inject, verify, link-health,
metrics, providers) is **[TS-ONLY]**.

### 0.3 Datastores

* PostgreSQL 16 + pgvector (`vector(384)`), pgcrypto.
* Qdrant collections `sf_posts`, `sf_paragraphs`: size 384, distance Cosine, payload index `site_id` (keyword);
  point id = Postgres row UUID; payload `{site_id, post_id, url}` (`shared/src/qdrant.ts`).
* Redis (BullMQ).

Core tables (from `db/migrations/001..011`): `sites(id, domain UNIQUE, wp_url, mode wp_rest|sitemap, api_key, settings jsonb)`,
`posts(id, site_id, wp_post_id, url UNIQUE(site_id,url), slug, title, content_html, content_text, content_hash, builder,
post_type, post_status, meta jsonb, word_count, cluster_id, pagerank, click_depth (-1 unknown), is_orphan, is_dead_end,
embedding vector(384), published_at, modified_at, crawled_at, +010: is_contextual_orphan, is_nav_only,
body_inbound_count, nav_inbound_count, +006: outbound/internal/broken/nofollow/redirect_link_count, has_breadcrumbs,
has_breadcrumb_schema, breadcrumb_report, link_health_checked_at)`,
`paragraphs(id, post_id, idx, text, hash, embedding, UNIQUE(post_id, idx))`,
`clusters(id, site_id, label, pillar_post_id, source auto|custom)`,
`keywords(id, post_id, keyword, source title|htag|meta|body|gsc, score, volume, position, striking, UNIQUE(post_id,keyword,source))`,
`link_edges(id, site_id, source_post_id, target_post_id, paragraph_id, anchor, anchor_type, composite_score,
score_breakdown jsonb, silo_topology, insert_module, status suggested|approved|rejected|applied|reverted|failed,
origin plan|manual|keyword_linker|existing|archive, snapshot_id, error, created_at, decided_at, applied_at,
UNIQUE(site_id, source_post_id, target_post_id, anchor))`,
`snapshots(id, post_id, content_html, reason, taken_at)`, `keyword_rules`, `ignore_keywords`, `link_health`,
`site_metrics_history`, `model_providers`, `models`, `model_chains`, `model_chain_rungs`, `model_call_log`, `job_runs`.

Note the edge uniqueness key **includes `anchor`** — the same source→target pair may exist several times. This is the
root of several bugs in §B.

---

## 1. Crawler

### 1.1 `crawler/wpCrawl.ts::crawlWpRest(site)` [TS-ONLY]

* `base = (site.wp_url ?? "https://"+domain)` with trailing `/` stripped.
* For kind in `posts`, `pages`: GET `${base}/wp-json/wp/v2/${kind}?per_page=100&page=N&_embed=false`, N=1.. until:
  HTTP 400 with `code == rest_post_invalid_page_number` → stop; other 400 → error; 404 on page 1 → kind absent;
  non-2xx → error; empty array or `< 100` items → stop.
* Per item → `CrawledPost`:
  `wp_post_id=id`, `url=link ?? base+"/?p="+id`, `slug`, `title=stripTags(title.rendered)`,
  **`content_html = content.rendered`** (not `raw`, no `context=edit`), `meta_description = yoast_head_json.description ??
  stripTags(excerpt.rendered)[:300]`, `post_type = type ?? post|page`, `post_status = status ?? publish`,
  `builder = detectBuilder(content_html, content.raw)`, `published_at = date_gmt+"Z"`, `modified_at = modified_gmt+"Z"`.
* Taxonomies `categories`, `tags`: GET `...?per_page=100&page=N&_fields=id,count,link,name,slug,description`, best-effort
  (any error/400/404 stops that taxonomy). Terms with no `link` or `count<=0` skipped. Upserted as posts row with
  `post_type='archive'`, `builder='archive'`, `content_text = name [+ " " + description]`,
  `content_html = "<p>" + text.replace(/[<>]/g,' ') + "</p>"`, hash = `sha256("archive:"+title+"\n"+text)`, skip if equal.
* `fetchWithRetry(url, retries=2)`: SSRF `assertPublicUrl` first, then undici fetch, timeout 30 s, UA
  `SiloForge/0.1 (+internal-linking)`, retry backoff 500 ms × (attempt+1) on throw only (not on 5xx).

### 1.2 `crawler/sitemapCrawl.ts::crawlSitemap(site)` [TS-ONLY]
Sitemapper on `/sitemap.xml` then `/sitemap_index.xml` (timeout 20 s; SSRF check only on the root sitemap URL).
For each same-domain URL (www-insensitive host equality): fetch, `extractMainContent`: title = `<title>` or first `<h1>`;
content = first non-empty of `article`, `main`, `.entry-content`; else `body` minus `nav, footer, header, aside, script,
style, noscript`. `wp_post_id = null`, `post_type='post'`, builder from full page.

### 1.3 `crawler/builder.ts` [TS-ONLY]
* `detectBuilder(html, raw?)`: `/elementor-widget|data-elementor-type/` → elementor; `/et_pb_/` → divi;
  raw contains `<!-- wp:` → gutenberg; `/class="[^"]*wp-block/` or html contains `<!-- wp:` → gutenberg; else classic.
* `normalizeUrl(raw)`: `host.lower().strip("www.") + path.rstrip("/") or "/"`; query/hash dropped; invalid → raw rstrip "/".
* `upsertCrawledPost(site, p)`: `hash = sha256_hex(content_html)`; if row exists with same hash → skip.
  `extractContent`: cheerio, remove `script,style,noscript`, `hTags` = trimmed texts of `h2,h3`, `text` = root text
  whitespace-collapsed, `wordCount = split(/\s+/).length`. `meta = {h_tags, description?, money_page:true if
  normalizeUrl(url) ∈ normalizeUrl(settings.money_page_urls)}`. UPSERT on `(site_id,url)`, `meta = posts.meta || EXCLUDED.meta`
  (jsonb merge), `published_at/modified_at = COALESCE(new, old)`, **`embedding = NULL`**, `crawled_at = now()`.

### 1.4 `crawler/ssrfGuard.ts` [TS-ONLY]
`assertPublicUrl(url)`: scheme must be http/https; host on `CRAWL_ALLOW_PRIVATE_HOSTS` (comma list, read live) → allowed;
literal IP → reject if blocked; hostname → `dns.lookup(all)` and reject if **any** address blocked; DNS failure/no records → reject.
Blocked IPv4: 0/8, 10/8, 127/8, 169.254/16, 172.16/12, 192.168/16. Blocked IPv6: `::`, `::1`, fc00::/7, fe80::/10,
IPv4-mapped `::ffff:a.b.c.d` and `::a.b.c.d` checked as IPv4. Malformed IP literal → blocked.

### 1.5 Paragraph chunking + SHA-1 canonicalization — `crawler/chunk.ts` + **[RUST-EXISTS] html.rs**

This is THE canonical splitter: paragraph `idx` must map back to the same block of `content_html` at insert time.

Algorithm (both implementations):
1. Parse as an HTML5 **fragment** (cheerio `load(html, null, false)` / scraper `Html::parse_fragment`).
2. Iterate **top-level element children only** (top-level bare text nodes are ignored). `visit(el)`:
   * `h1..h6` → skip (boundary, never a paragraph).
   * `ul`/`ol` → ONE block: text = join(" ", [normalize(li.text) for every **descendant** `li`, non-empty]); html = outer html.
   * `p`, `blockquote`, `pre` → block (text = normalize(el.text)).
   * `div, section, article, main, header, footer` → if no element children: block; else recurse into element children.
   * anything else (table, figure, script, style, iframe, form, custom tags…) → block.
3. `normalize(s)` = collapse all whitespace runs to one space, trim (JS `/\s+/g`; Rust `split_whitespace`).
4. Filter: drop text with length < **40 UTF-16 code units** (`MIN_CHARS = 40`).
5. Dedupe: `sha1_hex(utf8(text))` (lowercase hex, 40 chars); keep first occurrence only. Order preserved → `idx` 0..n-1.

Rust output per paragraph: `{text, html (outer), sha1}`; plus `links[]` (see §5) — `http_link_count`, `outbound_domains` (sorted).

`chunkPost(postId)` (DB): uses `analyzeHtmlFast` (Rust) for `{text, sha1}`; loads existing `(idx, hash) ORDER BY idx`; if
identical length and every `row.idx == i && row.hash == sha1[i]` → no-op. Else per paragraph
`INSERT ... ON CONFLICT (post_id, idx) DO UPDATE SET text, hash, embedding = CASE WHEN old.hash = new.hash THEN old.embedding ELSE NULL END`,
then `DELETE ... WHERE idx >= n`. Returns n. Called sequentially for every post (`plan.ts:110`).

Insert-time helpers (cheerio-only, **not** Rust): `splitParagraphBlocks`, `getParagraphHtml(contentHtml, idx)`,
`replaceParagraphInContent(contentHtml, idx, replacementHtml)` (re-collects blocks, `replaceWith`, re-serializes the
whole document).

`crawler/integrity.ts::checkParagraphIntegrity` — counts duplicate `(post_id, idx)` and posts with `max(idx) >= count(*)`;
warns only.

**Port requirement:** a single Rust function must serve both chunking and insertion, returning byte offsets of each
block in the original string so insertion is a byte-range splice (no re-serialization). Golden tests: `test/chunk.test.ts`,
`html.rs::canonicalizes_paragraphs_and_links`.

---

## 2. Embeddings — `embed/embedder.ts`, `embed/qdrantIndex.ts`, `embed/reaper.ts` [TS-ONLY]

### 2.1 Model and vector contract
* Model: **all-MiniLM-L6-v2**, 384-d, mean pooling, L2-normalized.
  * remote: OpenRouter `sentence-transformers/all-MiniLM-L6-v2` (`EMBED_REMOTE_MODEL`), POST `${OPENROUTER_URL}/embeddings`
    (default `https://openrouter.ai/api/v1`), body `{model, input: [texts]}`, headers `authorization: Bearer <key>`,
    `x-title: SiloForge`, timeout `EMBED_TIMEOUT_MS` = 20 000.
  * local: transformers.js `pipeline('feature-extraction', 'Xenova/all-MiniLM-L6-v2')`, `{pooling:'mean', normalize:true}`
    (lazy-loaded; cache volume `/models`).
* Rung chain: `EMBED_CHAIN` explicit list or from `EMBED_PROVIDER`:
  `remote-first` (default) = `openrouter,openrouter2,local`; `remote-only` = `openrouter,openrouter2`; `local` = `local`.
  `openrouter`/`openrouter1` uses `OPENROUTER_API_KEY`, `openrouter2` uses `OPENROUTER_API_KEY_T2` (skipped if empty).
  Empty chain → local.
* Per remote rung: attempts = `EMBED_RETRIES(1) + 1`; retry only if non-HTTP error or status ∈
  {408,409,425,429,500,502,503,504}; delay `EMBED_RETRY_DELAY_MS` 500. Then fall to next rung.
  All fail → throw `[embed] all rungs failed: ...`.
* Validation (`toUnitVector`): array, length == `EMBED_DIM` (384), every value finite number, norm > 1e-8, then divide by norm.
  Response `data[]` length must equal input count; if every row has numeric `index`, sort by index.
* Batching: `EMBED_BATCH_SIZE` = 16, sequential. Empty/whitespace text → `" "`. `EMBED_MAX_CHARS` (default 0 = off) truncates
  by UTF-16 units. Known: remote truncates at 256 word-pieces (~1300 chars), local at 512 → vectors differ for long inputs
  (cosine 0.88–0.93) unless `EMBED_MAX_CHARS≈1200`.
* `cosine(a,b)` plain cosine, 0 for zero vector; `parseVector('[..]')` pgvector literal.

Rust port: use `ort` or `candle` with the same ONNX export; tokenizer = `tokenizers` crate with the model's
`tokenizer.json`; truncate at 256 tokens (the model's `max_seq_length`) on every rung for consistency; mean-pool over
attention mask; L2-normalize. Must reproduce cosine ≥ 0.9999 vs current stored vectors.

### 2.2 `embedSite(siteId)`
1. `ensureCollections()`.
2. Posts with `embedding IS NULL` (ORDER BY id): text = `title + "\n" + content_text.slice(0, 2000)` (UTF-16 slice);
   embed all → Qdrant upsert (`sf_posts`, batches of 256, `wait:true`) → single PG transaction, `UPDATE posts ... FROM unnest`
   batches of 256.
3. Paragraphs with `embedding IS NULL` (ORDER BY post_id, idx): **first deletes ALL `sf_paragraphs` points for the site**,
   then embeds, upserts, updates PG in one transaction.

### 2.3 `findSimilarPostsBatch(siteId, postIds, k)`
Loads post vectors from PG, Qdrant `searchBatch` on `sf_posts` (batches of 128) with `limit k+1`, filter `site_id`,
drops self, keeps k; `post_id = payload.post_id ?? id`.

### 2.4 Reaper
Scroll Qdrant ids per site (page 1000, no payload/vector) for both collections, delete ids not in PG (chunks of 1000).

DB: posts.embedding, paragraphs.embedding; Qdrant both collections.

---

## 3. Keywords and GSC [TS-ONLY]

### 3.1 `keywords/extract.ts::extractKeywords(siteId)`
`extractTerms(text)` = npm `keyword-extractor` `extract(text, {language:'english', remove_digits:true,
return_changed_case:true, remove_duplicates:false})` then `len > 2`. **Output is unigrams** (stopword-filtered, lowercased).
Rust port must vendor the package's English stopword list verbatim for parity.

Layers / scores: title 1.0 (each distinct term), htag 0.8 (terms of `meta.h_tags.join(' ')`), meta 0.7 (`meta.description`),
body: term frequency over `content_text`, top 15 by count (stable sort; ties by first appearance), score `0.4 * count/maxCount`.
Keyword truncated to 255 chars; dedupe key `(post, source, keyword)`. Bulk upsert in one transaction, batches of 2000,
`ON CONFLICT (post_id, keyword, source) DO UPDATE SET score`. **Never deletes** keywords that disappeared.

### 3.2 `keywords/gsc.ts::pullGsc(siteId)`
Credentials: `settings.gsc.credentials` (string or object) else env `GSC_CREDENTIALS_JSON`; none/invalid → return 0.
Auth: RS256 JWT `{iss: client_email, scope: webmasters.readonly, aud: oauth2 token URL, iat, exp: iat+3600}` →
token exchange (grant_type jwt-bearer). Query `searchAnalytics/query` for `sc-domain:<domain>`, on error retry with
`https://<domain>/`: last 90 days, dimensions `[query, page]`, `rowLimit 5000` (no pagination).
Map `normalizeUrl(page)` → post. Upsert `keywords(source='gsc', score=min(1, ctr+0.1), volume=impressions,
position, striking = 4 ≤ position ≤ 10)`, one INSERT per row.

### 3.3 `intent/match.ts::matchIntent(siteId)` (exported, **never called** by any job)
For GSC keywords with volume > 0: embed the query; best post = argmax `cosine(q, post) * sqrt(volume / maxVolume)`;
log mismatches vs the ranking post. Writes nothing.

---

## 4. Link graph — `graph/engine.ts` + **[RUST-EXISTS] graph.rs**

### 4.1 URL identity
* `urlPathKey(raw, base?)`: `new URL(raw, base)`, pathname with trailing slashes stripped, `""`→`"/"`,
  `decodeURIComponent` (keep raw on failure), lowercase. Query and fragment ignored.
* `urlHostKey(raw)`: hostname lowercase, strip `www.`, no port.
* siteHosts = configured domain host (scheme/path/port/www stripped) ∪ hosts of all post URLs.

### 4.2 `loadGraph(siteId)`
* Nodes: every post of the site (all post types, including archives). Query has no ORDER BY.
* Edges: `SELECT DISTINCT source,target FROM link_edges WHERE status='applied' OR origin='existing'` (self-loops dropped),
  plus links parsed live via `html.analyze({html, baseUrl: post.url, siteHosts})` in chunks of 16 concurrent requests:
  keep links with `is_external === false` and `resolved_url != null`; target = `pathToPostId[urlPathKey(resolved_url)]`;
  skip self; record `{source, target, anchor: text[:255]}`.

### 4.3 `graph.metrics` (Rust, identical JS fallback)
Input `{nodes[], edges[{source,target}], roots[], alpha=0.85, max_iterations=100, tolerance=1e-6}`.
* Dedupe edges, drop self-loops; unknown node → error; duplicate node id → error.
* **PageRank**: if n==0 or no edges → all 0. Else init `1/n`; each iteration:
  `dangling = alpha * Σ rank[i] for i with outdeg 0`; `next[*] = dangling/n + (1-alpha)/n`;
  for each source with outdeg>0: `next[t] += alpha*rank[s]/outdeg(s)`; `err = Σ|next-rank|`; converged when `err < n*tol`.
  Non-convergence after max_iterations → **error** (fatal for the plan).
* WCC: BFS over out∪in edges; component ids 1.. in node order.
* Click depth: multi-source BFS along out-edges from valid roots (depth 0); no valid root → node 0. Unreached = -1.
* Per node: pagerank, component, click_depth, in_degree, out_degree, is_orphan (in=0), is_dead_end (out=0).
  Summary: nodes, edges, orphans, dead_ends, avg_depth = round2(mean over reached), components.

### 4.4 `buildGraph(siteId)`
1. Insert discovered links as `link_edges(status='applied', origin='existing', anchor, applied_at=now())`
   `ON CONFLICT DO NOTHING` (anchor_type takes the column default **'partial'**).
2. roots = posts where `isRootUrl(url)` = **path segment count ≤ 1**; none → first post.
3. Run graph.metrics.
4. Nav provenance: `link_health` rows with `placement ∈ {nav,header,footer,aside}` → distinct inbound sources per target.
   `classifyReachability`: hasBody = in_degree>0; hasNav = nav count>0; `isOrphan = !body && !nav`,
   `isContextualOrphan = !body`, `isNavOnly = !body && nav`.
5. One bulk `UPDATE posts ... FROM jsonb_to_recordset` with pagerank, click_depth, is_orphan, is_dead_end
   (+ provenance columns when migration 010 is present).

`getComponents(siteId)` reloads and re-parses the whole graph just to return component ids (used by metrics).

---

## 5. Link extraction details (Rust html.analyze links)
For every `a[href]` (trimmed, non-empty): `http_link_count += href starts with "http://"` (case-insens.);
resolved = absolute parse or `base.join(href)`, kept only for http/https; host = canonical (lowercase, trailing dot and
`www.` stripped, port excluded); `is_external = host ∉ internal_hosts` (internal = site_hosts ∪ base host) or null when
unresolved; text = normalized, first 255 chars; rel = lowercase whitespace tokens. JS fallback matches, except see §B
(code-point vs UTF-16 length).

`health/extractLinks.ts` (link-health variant, cheerio): skips `#…`, mailto/tel/sms/javascript/data/file/ftp/callto/
whatsapp/skype; honours `<base href>`; rel tokens split on whitespace or commas; placement = nearest ancestor
`nav|header|footer|aside` or role `navigation|banner|contentinfo|complementary`, else `body`; scope internal iff host ∈ aliases.

---

## 6. Clustering — `silo/cluster.ts` + **[RUST-EXISTS] kmeans.rs**

* Input posts: `embedding IS NOT NULL AND (cluster_id IS NULL OR cluster_id NOT IN custom clusters)` ORDER BY id.
  All post types (archives included). < 2 posts → skip.
* `k = min(arg ?? defaultK(n), n)`, `defaultK(n) = clamp(round(sqrt(n/2)), 2, 25)`.
* Seed string = siteId. `hashSeed` = FNV-1a 32-bit over **UTF-16 code units**: `h=2166136261; h ^= cu; h = h*16777619 (u32 wrap)`.
* PRNG mulberry32 (u32): `s += 0x6D2B79F5; t = (s ^ s>>15) * (1|s); t = (t + ((t ^ t>>7) * (61|t))) ^ t; return (t ^ t>>14) / 2^32`.
* k-means++: first centroid = `vectors[floor(rng*n)]`; while < k: `d_i = min_c ||v_i - c||²`; `total = Σd`;
  total==0 → random pick; else `r = rng*total`, walk `r -= d_i` and pick first i with `r <= 0`; if none hit → **index 0**.
* Lloyd: up to 50 iterations; assign to nearest centroid (strict `<`, lowest index wins ties); stop if no change and iter>0;
  recompute means; empty cluster keeps previous centroid. Squared Euclidean (vectors are unit-normalized).
* Persist: `DELETE clusters WHERE source='auto'` (FK sets posts.cluster_id NULL), then per non-empty cluster: insert
  `clusters(label, source='auto')`, `UPDATE posts SET cluster_id`, pillar = member minimizing `sqDist(v, centroid)`;
  if |Δdist| ≤ 1e-6 tie-break by pagerank desc.
* Label: words from member titles split on `[^a-z0-9']+`, len ≥ 3, not in STOPWORDS (the/a/an/and/or/but/of/for/to/in/on/at/
  by/with/is/are/was/were/be/been/it/its/this/that/these/those/you/your/we/our/how/what/why/when/where/which/who/best/top/
  guide/vs/from/as/can/do/does/not/no), top 3 by frequency (stable), joined by space; else `"cluster"`.
* Golden: `kmeans.rs::matches_the_typescript_seeded_fixture` (assignments [1,1,0,0]); `hash_seed("😀") = 3409036472`.

`silo/customClusters.ts`: export `{clusters:[{label, pillar_url, member_urls (sorted)}]}`; import deletes all custom
clusters then inserts each, matching posts by exact URL, sets pillar.

---

## 7. Silo topologies — `silo/topologies.ts` [TS-ONLY]

`edgesForTopology(topology, members, pillar, pagerankById)` where `ms = members \ {pillar}` (order = DB `ORDER BY id`):
* **reverse**: every m → pillar.
* **serial**: ms[0]→ms[1]→…→ms[last]→pillar.
* **priority**: every m → pillar; pillar → top-3 ms by pagerank desc (stable).
* **bidirectional**: every m → pillar and pillar → every m.
* **circle**: ring [pillar, ...ms], i → (i+1) mod len.

`buildTopologyEdges(siteId, topology, limits)`: for every cluster with a pillar, members `cluster_id = c AND id <> pillar
ORDER BY id`; candidate rank = target pagerank; site-wide `capCandidatesPerSource` with `getExistingLinkCounts`; insert each
kept as `link_edges(anchor='', status='suggested', origin='plan', silo_topology)` `ON CONFLICT (site,source,target,anchor) DO NOTHING`.

`buildArchiveSilo(siteId, topN=5, limits)`: per archive page: `findSimilarPosts(archive, topN + #archives)` minus archives,
first 5; fallback same-cluster non-archives by pagerank desc limit 5; for target i: rank = len - i, add archive→t and
t→archive; density-cap; insert with `origin='archive'`.

Semantic candidates (`plan.ts:39`): `SEMANTIC_K=5`, keep `score > 0.5`, not self, still a current post, not already
present in **any** status for that (source,target) direction; bulk insert `origin='plan'`.

---

## 8. Density rules — `rules/density.ts` [TS-ONLY]

Defaults `LINK_DENSITY = {maxPerPost:10, minPerPost:3, maxNewPerPass:5, overloadThreshold:100}`; settings
`max_links_per_post, min_links_per_post, max_new_links_per_pass, link_overload_threshold` clamped (trunc) to
[1,200], [0,200], [1,200], [1,10000]; `minPerPost ≤ maxPerPost`, `maxNewPerPass ≤ maxPerPost`.

`capCandidatesPerSource(candidates, existing, limits)`: per source, `budget = max(0, min(maxPerPost - existing, maxNewPerPass))`;
keep top-`budget` by `composite_score` desc, ties by input index; output preserves input order.

`getExistingLinkCounts`: `COUNT(*)` of edges with `status IN (applied, approved) OR origin='existing'` per source.

`applyDensityCaps(siteId, settings)`: suggested edges `ORDER BY composite_score DESC, id`, cap, DELETE dropped.

`findUnderLinkedPosts` (publish, non-archive, committed outbound < min) and `findOverloadedPosts`
(`outbound_link_count > overloadThreshold`, tolerant of missing column 42703).

---

## 9. Composite scorer — `score/composite.ts`, `score/weights.ts` [TS-ONLY]

### 9.1 Weights (LOOM defaults; sum 1.0)
| dim | w | dim | w |
|---|---|---|---|
| semantic | 0.22 | graph | 0.10 |
| orphan | 0.08 | money | 0.10 |
| depth | 0.06 | gsc | 0.08 |
| tier | 0.06 | authority | 0.10 |
| cluster | 0.06 | placement | 0.10 |
| velocity | 0.04 | | |

### 9.2 Inputs (`DimInputs`) and the 11 dimensions (each clamped/defined in [0,1])
`incoming` = number of committed inbound **rows** (status applied OR origin existing) to target.
1. **semantic** = clamp01(cosine(source post emb, target post emb)) (computed in SQL as `1 - (a <=> b)`; null → 0).
2. **orphan** = incoming==0 → 1.0; <3 → 0.5; <5 → 0.2; else 0.
3. **depth** (target click_depth d): null or d<0 → 1.0; d>4 → 1.0; d==4 → 0.7; d==3 → 0.3; else 0.
4. **tier**: targetTier < sourceTier → 1.0; equal → 0.3; else 0.1.
   `tierOf(url, meta.tier)` = meta.tier if set, else path segments s: s==0 → 0, else `min(3, s+1)` (so 1 segment → 2), invalid URL → 3.
5. **cluster**: target is pillar AND same cluster (non-null) → 1.0; same cluster → 0.8; target clustered elsewhere → 0.2; target unclustered → 0.
6. **velocity**: `ageDays = max(1, targetAgeDays ?? 90)`, `ageMonths = max(1, ageDays/30)`, `lpm = incoming/ageMonths`;
   incoming==0 → 0.8; ageDays>180 && lpm<0.5 → 0.7; ageDays>90 && lpm<1 → 0.5; lpm<1 → 0.2; else 0.
   ageDays = (now − published_at)/86 400 000, min 1; null published_at → null.
7. **graph** (additive, clamp01): +0.4 if target is_orphan; +0.3 if avgPR>0 && targetPR < 0.5·avgPR;
   −0.15 if target is_dead_end && targetPR > avgPR; +0.15 if same weak component; +0.1 if avgPR>0 && sourcePR > 2·avgPR.
   avgPR = mean of finite pageranks over all site posts. Components from union-find over committed edges
   (`parent[targetRoot] = sourceRoot`, path compression).
8. **money** (only if target or source money): if target money: +0.5 + priority(=1)·0.05; `deficit = max(0, goal(=10) - incoming)`;
   deficit>0 → +min(0.25, 0.05·deficit) else −0.2. If source money and target not: −0.3. clamp01.
9. **gsc**: !gscConnected or striking==0 → 0; else clamp01(0.5 + 0.25·(striking−1)). gscConnected = any gsc keyword on site.
10. **authority** (clamp01): if target clustered: +min(0.4, 0.1·clusterInbound) (committed inbound rows from same-cluster sources);
    PR ratio t/avg: ≥3 → +0.3, ≥1.5 → +0.2, ≥1 → +0.1; non-gsc keyword count: ≥5 → +0.3, ≥3 → +0.2, ≥1 → +0.1.
11. **placement**: requires bestParagraph (1-based) > 0 and paragraphSim > 0.3; `totalParas = (word_count || 500)/100`;
    `ratio = bestParagraph / max(1, totalParas)`; ≤0.33 → min(1, 1.5·sim); ≤0.66 → min(1, sim); else min(1, 0.5·sim).
    Best paragraph = source paragraph with max cosine to the **target post** embedding (LATERAL `ORDER BY <=> LIMIT 1`).

### 9.3 Adaptive renormalization (v1.4.0)
Availability per **site**: `gsc` = any gsc keyword; `money` = any post `meta.money_page === true`; `tier` = >1 distinct `tierOf`.
`effectiveWeights(av)`: excluded = unavailable dims among {gsc, money, tier}; if none → WEIGHTS. Else
`scale = 1/(1 − Σ w_excluded)`; every other weight `w' = round4(w·scale)`, excluded = 0; then
`semantic = round4(semantic + (1 − Σw'))` to absorb residue. Example (all three excluded, scale 1/0.76):
semantic .2895, orphan .1053, depth .0789, cluster .0789, velocity .0526, graph .1316, authority .1316, placement .1316.
`compositeScore = round4(Σ dim·w')`. `round4(x) = Math.round(x·10000)/10000`; in Rust use `(x*1e4 + 0.5).floor()/1e4`
for bit-parity (JS rounds .5 toward +∞). Breakdown persisted with `weights` = effective weights.

### 9.4 `scoreCandidates(siteId)` DB flow
Selects edges `status='suggested' AND composite_score = 0` with SQL cosine and best paragraph; loads posts, committed edges,
pillars, keyword aggregates; computes; one bulk `UPDATE link_edges SET composite_score, score_breakdown, paragraph_id`.
Golden tests: `test/composite.test.ts`, `test/renormalization.test.ts`.

---

## 10. Anchor generation — `anchor/nlp.ts`, `anchor/classify.ts`, `anchor/cannibalization.ts`, `rules/ignore.ts` [TS-ONLY]

### 10.1 Noun-phrase extraction (`extractNounPhrases`)
wink-nlp + `wink-eng-lite-web-model` (UD POS tags). Maximal runs of consecutive tokens with POS ∈ {ADJ, NOUN, PROPN}.
For each run with ≥2 tokens: for len = min(run, 6) down to 2, for start 0..: phrase = tokens joined by single space;
skip if first word (lower) ∈ BOILERPLATE_LEAD {analysis, terms, disclaimer, disclosure, affiliate, table, contents};
dedupe case-insensitively; stop at **40** candidates (document order). Rust port: use a POS tagger with UD tags
(e.g. `nlprule`, or an ONNX POS model); must produce the same runs on the test fixtures in `test/anchor.test.ts`.

### 10.2 Phrase scoring (`scorePhrase(phrase, targetTokens)`)
`tokenSet(t)` = lowercase split on `[^a-z0-9']+`, keep len > 2, as a set. `weight(tok)` = map weight or 1/0 for set.
`content` = tokens of phrase; `descriptive` = content \ LISTICLE_WORDS
(top, best, our, pick, picks, choice, choices, review, reviews, guide, guides, list, ultimate, greatest, favorite,
favourite, recommended, roundup, compared, comparison, cheap, cheapest).
`overlapAll = Σ weight(content)`, `overlapDesc = Σ weight(descriptive)`, `wordCount` = whitespace words of phrase.
Reject if content empty, overlapDesc == 0, or (descriptive < 2 && wordCount < 3).
`lengthScore` = 6 for 3–6 words, −2 for 2, −5 otherwise. `genericPenalty = 5·(|content| − |descriptive|)`.
`score = 20·overlapDesc + 3·overlapAll + 2·|descriptive| + lengthScore − genericPenalty`.

### 10.3 `pickAnchor(paragraph, title, keywords[], weighted[])`
target tokens: if no weighted → tokenSet(title + " " + keywords.join(" ")); else weighted map max-merge of
{title:1, keywords:1, each weighted (GSC striking, weight 1.5)}. Candidates = noun phrases whose lowercase is a substring of
the lowercase paragraph; best by score (first wins ties) → `{anchor, inParagraph:true}`.
Else first keyword with `length > 3` that occurs as substring → that keyword (often ONE word). Else title if substring.
Else `cleanTitle(title)` (or first 8 words, or title) with `inParagraph:false`.

`cleanTitle(title, max=6)`: collapse spaces; drop leading tokens whose `norm` (lower, `[^a-z0-9]` removed) is empty, numeric,
listicle word, or the/a/an; cut at first clause breaker {that, which, who, whom, where, when, because, so} when index > 0;
drop year tokens `^(19|20)\d{2}$`; pop trailing stop words {for, the, a, an, of, to, and, or, with, in, on, at, by, your,
our, my, their}; take first 6 words.

`pickAnchorAcrossParagraphs(paras)`: try paragraphs in given order (top-5 by cosine to target post), return first
in-paragraph pick with its `paragraphIdx`; else the first fallback.

### 10.4 Classification (`classifyAnchor(anchor, topKeyword, brand)`), precedence
1. **naked**: no whitespace and matches `^(?:https?://)?((?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z]{2,})(?::\d{1,5})?(?:[/?#]\S*)?$`
   and (has scheme, or host starts `www.`, or TLD ∈ NAKED_BARE_TLDS {com net org io co ai app dev info biz edu gov shop store
   blog news me tv us uk ca au de fr es it nl se no eu in jp}).
2. **branded**: brand non-empty and regex `(^|[^a-z0-9])<brand>([^a-z0-9]|$)`; `brandFromDomain` = domain lower, strip `www.`,
   first label.
3. **cta**: not generic; first word (letters only) ∈ {read, see, check, learn, discover, view, shop, compare, find, get} and
   matches one of the 11 CTA_PATTERNS (`classify.ts:102-125`, copy verbatim).
4. **exact**: anchor == topKeyword. 5. **partial**: anchor ⊂ kw or kw ⊂ anchor (substring).
6. **generic**: in {click here, read more, this article, learn more, here} or (≤2 words and any ∈ {here, click, read, more, learn, check}).
7. **lsi**: otherwise.
`topKeyword` = first of target keywords `ORDER BY score DESC` (no tie-break), top 10 kept.

### 10.5 Guards
* **Site-wide over-optimization** (`applyOverOptimizationGuard`): pool = existing anchors (status suggested/approved/applied,
  anchor≠'') + new items. While share(exact) > 0.40: first new item with type exact having an alternate of another type →
  replace with first `partial` alt, else first `lsi`, else first non-exact; remove alt from list. Same for generic > 0.30.
* **Per-target diversity** (`applyDiversityGuard`): group by target; skip groups < 3; up to |group| iterations: types with
  share > 0.70; take the first such type (in order of first appearance); first new victim with a differently-typed alt →
  swap to the first alt of a different type.
* Alternates per edge = up to 4 other noun phrases of the chosen source paragraph + cleaned title, each classified.
* Ignore list: `isIgnored(text)` = any ignore phrase (lowercased) is a **substring**.
* Persist: for attempts [chosen, ...alternates] minus ignored: `UPDATE link_edges SET anchor=attempt[:255], anchor_type,
  insert_module=predicted` — on any exception try the next (unique-key collision expected).
  `predicted = pickModule(bestParagraphIdx==null ? 0 : composite·100, settings.insert_modules, preset)`.
  Then update `paragraph_id` for edges whose winning paragraph changed; finally DELETE suggested edges with blank anchors.

### 10.6 Cannibalization report
`getCannibalizationWarnings`: exact anchors (suggested/approved/applied) pointing to >1 distinct target, grouped by anchor.
`siteAnchorDistribution`: counts by type and by lowercase anchor.

---

## 11. Insertion — `pipeline/insert.ts`, `pipeline/llm.ts` [TS-ONLY]

* `pickModule(score100, allowed=['native'], preset)`: shift = conservative +10, aggressive −10, else 0; first of
  native ≥60, rephrase ≥40, context ≥30 that is allowed and clears `min+shift`; else `content_gap`.
* `buildInsertion(edge)`: loads settings, paragraph (by `paragraph_id`), target url/title, source content_html;
  paragraph html = `getParagraphHtml(content, idx)` or `<p>escaped text</p>`. Native → `wrapAnchor`; on null fall through to
  `pickModule` without native → rephrase/context via LLM, else content_gap.
* **`wrapAnchor(paragraphHtml, anchor, url)`** (anchor-nest guard): needle = lowercase anchor; DFS over text nodes in document
  order, skipping `<a>` subtrees and script/style; first text node whose lowercase contains the needle (plain `indexOf`,
  **no word boundary**) is replaced by `escape(before) + <a href="escapeAttr(url)">escape(match)</a> + escape(after)`;
  returns re-serialized paragraph HTML or null.
* `appendSentence(p, s)`: append `" " + s` inside the first root element.
* LLM (`llm.ts`, via provider chain §14): maxTokens 2000, temperature 0.3.
  Rephrase prompt: system "expert content editor … PRESERVE all existing HTML tags … return ONLY the rewritten paragraph HTML";
  user asks to include `<a href="{url}">{anchor}</a>`, target title, keep meaning/tone/length. Reply stripped of code fences,
  used as-is. Context prompt: one transition sentence containing the link; if reply lacks `<a\s` →
  `reply + " See <a href=url>anchor</a>."` (unescaped).

## 12. Injection, snapshot, undo — `pipeline/inject.ts` [TS-ONLY]

`injectEdges(siteId, edgeIds, verifyOpts?)`, sequential over edges, each `status='approved'` only:
1. `buildInsertion`; content_gap/null → set `insert_module='content_gap'`, count `marked`.
2. No WP creds (`settings.wp_user` + `wp_app_password`) → set `insert_module`, keep approved (plugin pull), `marked`.
3. Push mode: load source post; require `wp_post_id`; `newContent = replaceParagraphInContent(content_html, idx, html)`;
   `INSERT snapshots(post_id, content_html, 'pre-inject')`; PUT `${base}/wp-json/wp/v2/{posts|pages}/{id}` body
   `{content: newContent}`, Basic auth, 30 s timeout; update `posts.content_html`; edge → applied (+snapshot_id, applied_at).
4. Unless `settings.skip_inject_verify`: `verifyInjectedPost`; ok → verified; inconclusive → keep, write `error='verify_skipped…'`;
   damage → `restoreFromSnapshot` (WP PUT + local), edge → failed.
5. Exceptions → edge failed with error.

`undoEdges`: for applied edges with snapshot → restore snapshot (WP PUT if creds, then local `content_html`) → status reverted.

Plugin-pull mode (`api/routes/suggestions.ts`): serves approved edges `{edge_id, wp_post_id, target_url, anchor,
paragraph_index (= paragraphs.idx), insert_module ?? 'native', replacement_html: null}`; plugin splices in PHP and acks
(`applied`/`failed`). The PHP splitter must reproduce §1.5 exactly.

## 13. Post-injection verify — `pipeline/verify.ts` [TS-ONLY]
Constants: WORD_DRIFT_PASS 0.02, WORD_DRIFT_FAIL 0.05, UNLINKED_LOSS_FAIL 0.05, UNLINKED_LOSS_MIN_WORDS 3,
BASELINE_MIN_WORD_RATIO 0.85, BASELINE_MIN_RETAINED 0.6; attempts 3, backoff 1500·2^(n−1) ms, timeout 20 s, UA
`SiloForge/0.2 (+post-injection-verify)`; request headers `cache-control: no-cache`, `pragma: no-cache`; redirect follow.
* `profileHtml` (parse5 fragment, ignoring script/style/noscript/svg/template subtrees): words, textLen, tag histogram,
  normalized hrefs, anchors (href, normalized text: lowercase, curly quotes→straight, en/em dash→`-`, `…`→`...`), unlinkedWords.
* Content region of live page: first non-empty of `.entry-content, .post-content, .wp-block-post-content,
  .elementor-widget-theme-post-content, .td-post-content, .article-content, article, main, body`.
* `compareProfiles(intended, rendered, href, anchor)` failures: link-missing; anchor-duplicated (>1 hits with identical text);
  tag-loss; href-missing (multiset, excluding expected); word-loss (shorter by >5%); text-loss (>5% chars);
  unlinked-text-loss (≥3 words and >5%). Warnings for additions.
* `checkBaseline`: word ratio ≥0.85 and 3-word shingle coverage of baseline ≥0.6.
* `evaluateFreshness`: cf-cache-status (fresh ∈ MISS/EXPIRED/DYNAMIC/BYPASS/REVALIDATED), x-litespeed-cache (hit=stale),
  x-cache (contains hit=stale; miss/bypass/dynamic=fresh), `age>0`=stale, none=fresh.
* Status 429/5xx/fetch error → transient retry; other non-200 → failure. Passing render (even cached) → ok. Failures on stale → retry;
  exhausting → inconclusive+stale.

## 14. Model providers — `providers/*` [TS-ONLY]
Kinds: openai-compatible, anthropic, openrouter, ollama, custom. Chain resolution: site active chain → global active chain →
env fallback (chat only): `DARIO_URL` (default `http://site01.example:3460/v1`), models `DARIO_MODEL_PRIMARY`
(`MiniMax-M3`) then `DARIO_MODEL_FALLBACK` (`claude-sonnet-5-5`), no auth. Requests: openai `/chat/completions`
`{model, messages, max_tokens, temperature}`; anthropic `/v1/messages` with `x-api-key`, `anthropic-version`, system split
out; ollama `/api/chat` `stream:false`. Embeds: `/embeddings` or ollama `/api/embeddings` per text. Default timeout 120 s,
default retries 0, retryable statuses as §2.1; empty chat text = failure; embeds validated to INDEX_DIM 384. Calls logged to
`model_call_log` best-effort. API keys stored encrypted (`api_key_enc`). **`runEmbed` is not used by the embedding pipeline.**

## 15. Rules — keyword linker [TS-ONLY]
`runKeywordLinker`: for each active rule not ignored: target = post with `url === rule.target_url` (exact string); sources =
published posts ≠ target whose `content_text` matches `(^|[^a-z0-9])<kw>([^a-z0-9]|$)` (case-insens.); up to `max_links`;
paragraph = first paragraph `text ILIKE '%kw%'` by idx; insert edge `anchor=kw, anchor_type=rule.anchor_type,
composite_score=0.5, score_breakdown={"keyword_rule":true}, origin='keyword_linker'`.

## 16. Link health — `health/*` [TS-ONLY]
Defaults: concurrency 2, intervalCap 4 per 1000 ms (p-queue strict) + AdaptiveThrottle (halve cap on 429/503 or timeout
pressure: 3-streak or >10% of last 50 with ≥10 samples), maxHops 10, chain deadline 60 s, maxUrls 20 000, body cap 2 MB.
Agent: 4 connections, headers 20 s, body 30 s, connect 8 s, retry interceptor on 429 (2 retries, 500–5000 ms, Retry-After).
traceRedirects: SSRF guard per hop, HEAD first (GET for internal targets to capture body), GET fallback on
{400,401,403,405,406,409,429,500,501,502,503}; redirects 301/302/303/307/308 (temp: 302/307); loop detection on normalized
URL. Failure kinds: none, not_found(404), gone(410), loop, hop_cap, server_error(5xx), blocked(401/403), rate_limited(429),
timeout, dns, connection, other; broken = {not_found, gone, dns, loop}; unverified = {timeout, rate_limited, connection,
blocked, hop_cap}. Targets deduped by `normalizeTargetUrl` (fragment dropped, host lower, default port removed).
Post-drain sequential retry of timeouts. Per-link upsert into `link_health` (unique site,post,href,anchor), per-post rollups,
prune rows `checked_at < run start`, then `checkSiloDrift`. Breadcrumbs: JSON-LD BreadcrumbList (walk depth 8) + visible
selectors (`breadcrumbs.ts:17-29`) containing ≥1 `<a>`; mismatch when exactly one is present.

## 17. Metrics — `metrics/history.ts` [TS-ONLY]
Health score = `clamp(round(100·(1 − 0.5·orphanRatio − 0.2·deadEndRatio − 0.3·(1 − min(1, avgLinks/5)))), 0, 100)`.
Snapshot jsonb to `site_metrics_history`; deltas with per-metric direction table (`history.ts:33-62`); pct null when previous 0.

## 18. Determinism requirements for the port
1. Every DB read that feeds an order-sensitive algorithm must have an explicit `ORDER BY` (TS relies on implicit order in
   several places — see §B-22). Canonical: posts `ORDER BY id`, edges `ORDER BY id`, keywords `ORDER BY score DESC, keyword`.
2. k-means: identical FNV-1a/mulberry32 sequences (already pinned by Rust tests); vector order = `ORDER BY id`.
3. PageRank summation order: iterate sources in node order, targets in insertion order (float parity with existing values).
4. Rounding: JS `Math.round` semantics; 4-decimal scores; 2-decimal avg depth.
5. Text lengths in UTF-16 units wherever TS uses `.length`/`.slice` (MIN_CHARS 40, 2000-char post text, 255/300 truncations).
6. SHA-1 over UTF-8 of the normalized text; whitespace class must match JS `\s` (includes U+FEFF and U+00A0; Rust
   `char::is_whitespace` excludes U+FEFF) — implement a custom predicate.
7. Weights snapshot stored per edge for reproducibility.

---

## B. Bugs, loopholes, bottlenecks, ghost issues

Severity: **C** critical (data loss / wrong output at scale), **H** high, **M** medium, **L** low.

### Data-integrity / correctness

1. **C — Rendered HTML is written back as post source.** `crawler/wpCrawl.ts:77` stores `content.rendered`;
   `pipeline/inject.ts:50-55,145-156` PUTs that (modified) rendered HTML as `content`. Every push permanently bakes the
   `the_content` filter output into the post: Gutenberg block comments lost (post becomes "Classic" HTML), shortcodes
   replaced by their output, oEmbeds frozen, TOC/ad/related-post/lazy-load plugin markup duplicated on the next render.
   `verify.ts` treats additions as warnings, so the damage is invisible. Port must crawl with `context=edit` (raw) and splice
   into raw content (block-aware), or fetch raw immediately before writing.
2. **C — Lost updates / clobbering live edits.** `inject.ts:131-156` uses `posts.content_html` from the last crawl, with no
   re-fetch, no `modified_gmt` precondition, and no lock. Editor changes since the crawl are overwritten; two inject jobs on
   the same post (2 replicas × concurrency 2, `worker/src/index.ts:90`) overwrite each other. `undoEdges` (`inject.ts:248-274`)
   restores a full snapshot, clobbering later edits as well.
3. **C — Undo/rollback snapshots are per edge, not per post.** Several edges in one post each snapshot the then-current HTML
   (`inject.ts:149-152`). Undoing an earlier edge restores HTML that predates later edges, which stay `status='applied'`
   (ghost applied links); verify-rollback of edge k also wipes edges k+1..n injected before it… and vice versa. Also each edge
   triggers its own WP PUT and full verify — N round trips per post instead of 1.
4. **C — LLM output is published unvalidated.** `llm.ts:65` returns whatever the model says (refusals, explanations, extra
   paragraphs, `<script>`); `llm.ts:96-98` appends the link to any text, including refusals, and interpolates `anchor`/`url`
   without escaping (stored-XSS vector via prompt injection from crawled content). Verify compares against the *intended*
   HTML (which already contains the garbage) so it passes. Require: exactly one new `<a>` with the expected href, anchor
   text present, shingle coverage of original ≥ 0.8, tag multiset preserved, HTML sanitized, single block.
5. **H — Paragraph index mismatch risk between three splitters.** chunkPost uses Rust (`chunk.ts:127`), insertion uses cheerio
   (`chunk.ts:94-110`), JS fallback uses **code points** (`rust/fallback.ts:67` `[...text].length`) vs UTF-16 in Rust/chunk.ts;
   whitespace classes differ (U+FEFF). No check that the block at `idx` still has `paragraph.hash` before replacing
   (`insert.ts:131`, `chunk.ts:103-109`) → wrong paragraph rewritten when content changed after planning or splitters diverge.
   Additionally a rephrase/context edit can change dedupe outcomes or block counts, shifting indices for later edges in the
   same batch (paragraph rows are not re-chunked after inject).
6. **H — Re-plans re-suggest pairs already linked/rejected.** Topology/archive inserts dedupe only on anchor `''`
   (`topologies.ts:127-135, 143-151`). After a first plan assigns anchors, every re-plan inserts the same (source,target) again;
   anchor generation then collides on the best anchor and stores an alternate → duplicate suggestions for pairs that are
   applied (duplicate links in one post), approved, or **rejected** by the operator. Only `addSemanticCandidates` checks all
   statuses (`plan.ts:44-57`).
7. **H — Density cap runs before scoring.** `plan.ts:175` (`applyDensityCaps`) precedes `scoreCandidates` (`plan.ts:180`), so all
   new candidates have `composite_score = 0`; "drop lowest score first" (`density.ts:177-185`) degenerates to `ORDER BY id`
   (random UUID) and always drops new candidates before previously scored backlog. The pagerank ranking used in
   `topologies.ts:119` is discarded here.
8. **H — Click depth is meaningless on WP permalink sites.** `graph/engine.ts:263-270` treats every URL with ≤1 path segment as
   a BFS root; with `/%postname%/` every post is a root → depth 0 everywhere → the `depth` dim is 0 for all edges (0.06 dead
   weight never renormalized) and `avg_click_depth` metrics are wrong. Root should be the homepage (+ nav targets).
9. **H — Anchor classification is broken by unigram keywords.** `keywords/extract.ts:23-30` yields single words; topKeyword
   (`nlp.ts:305`, ties at score 1.0 with no tie-break → nondeterministic) is therefore one word, so almost every anchor
   containing it is `partial` (audit: 46k partial / 4k exact / 7 branded). `exact` mostly comes from the keyword fallback
   (`nlp.ts:226-229`, `kw.split(/\s+/).length >= 1` is always true) which emits **one-word anchors**. The 40% exact guard is
   effectively never exercised.
10. **H — Existing links pollute the anchor pool.** `buildGraph` inserts discovered links without `anchor_type`
    (`graph/engine.ts:377-388` → default `'partial'`); `generateAnchors` feeds them into both guards (`nlp.ts:359-363`),
    diluting exact/generic shares and per-target diversity (~16k such rows per audit).
11. **H — Stale `origin='existing'` edges are never removed.** Links deleted from content stay forever and keep counting as
    inbound/outbound (scorer, density budget, graph edges, silo drift). After undo, a discovered copy with a case-different
    anchor survives as a ghost link.
12. **H — Silo drift check is a tautology.** `silo/drift.ts:41` `OR live.id = e.id` with `live.status='applied'` always matches
    the edge itself → `drifted` is always 0 (and existing rows are never pruned anyway, #11).
13. **M — Scores are computed once and never refreshed.** `composite.ts:315` scores only `composite_score = 0`; suggested edges
    keep stale orphan/velocity/graph values across plans; predicted `insert_module` is never updated; an edge that genuinely
    scores 0 is rescored every run.
14. **M — Inbound counts count rows, not distinct sources** (`composite.ts:357-363`, `density.ts:150-155`): the same pair with
    several anchors (applied + existing copy) double-counts.
15. **M — Placement mixes units** (`composite.ts:139-141`): canonical paragraph index (≥40-char, deduped blocks) divided by
    `word_count/100`; long-paragraph posts get skewed ratios.
16. **M — Tier availability is coarse** (`composite.ts:374-378`): homepage (0) + posts (2) + archives (3) makes `tier`
    "available" although nearly every candidate pair is same-tier 0.3. `tierOf` never yields 1 (`composite.ts:228`).
17. **M — Predicted module ignores `inParagraph`** (`nlp.ts:443-447`): title-fallback anchors (not in paragraph, cannot be wrapped)
    are predicted `native`.
18. **M — `wrapAnchor` matches inside words and inside non-prose.** `insert.ts:62` plain substring (e.g. `bike lock` →
    `<a>bike lock</a>s`, rule keyword "cart" in "cartoon" — same ILIKE in `keywordLinker.ts:69-73`, unescaped `%`/`_`);
    guard skips only `a/script/style` (`insert.ts:72-75`) so links land in `<pre>/<code>`, `<button>`, `<option>`,
    `<figcaption>`, table headers. Phrases spanning inline tags (`best <b>bike</b> locks`) are never found.
19. **M — Chunker turns non-prose into paragraphs**: top-level `<script>`, `<style>`, `<pre>`, tables, forms become blocks
    (`chunk.ts:55-56`, `html.rs:108`) → embedded and used for anchor search.
20. **M — Keyword linker bypasses density limits and scoring**, matches target by exact URL string (`keywordLinker.ts:55`),
    and `isIgnored` is a raw substring (`ignore.ts:17`: ignoring "car" blocks "scar", "carpet").
21. **M — `pullGsc` token failure aborts the whole plan** (`gsc.ts:128` uncaught), while bad JSON is silently skipped;
    `rowLimit 5000` without pagination (`gsc.ts:77`); old GSC rows never expire.
22. **M — Nondeterminism from missing ORDER BY**: `generateAnchors` edges (`nlp.ts:288-296`) decide which item the guards
    downgrade first; `loadGraph` posts (`graph/engine.ts:280`) decide component numbering and the fallback root; keyword
    ties (`nlp.ts:305`).
23. **L — `/?p=123` links resolve to the homepage**: `urlPathKey` drops the query (`graph/engine.ts:47-61`); `wpCrawl.ts:80`
    fallback URLs collide on path `/`.
24. **L — k-means++ draw fallback picks index 0** when float residue keeps `r > 0` (`kmeans.rs:117-125`,
    `cluster.ts:65-73`) → possible duplicate centroid / empty cluster. Keep for parity only behind a flag.
25. **L — `matchIntent` volume factor is constant per query** (`intent/match.ts:26-31`) so it cannot change the argmax; the
    function is never called; `Math.max(...kws)` (`match.ts:60`) overflows the stack on very large keyword sets.

### Security loopholes

26. **H — SSRF via redirects.** `fetchWithRetry` (`wpCrawl.ts:35`) and verify (`verify.ts:719`) use `redirect: 'follow'`
    after checking only the first URL; Sitemapper fetches child sitemaps unchecked (`sitemapCrawl.ts:16`). DNS is resolved
    twice (check then fetch) → rebinding TOCTOU. Only `traceRedirects` guards every hop.
27. **M — Incomplete blocklist** (`ssrfGuard.ts:97-120`): missing 100.64/10 (CGNAT), 192.0.0/24, 198.18/15, 224/4, 240/4,
    255.255.255.255, NAT64 64:ff9b::/96, 6to4 2002::/16 embedding private v4.
28. **M — Stored XSS through LLM modules** (see #4).

### Bottlenecks / high CPU in bulk linking

29. **H — `generateAnchors` blocks the event loop.** Per edge: up to 5 `pickAnchor` calls, each a full wink parse
    (`nlp.ts:262`), plus another parse for alternates (`nlp.ts:392`); the same source paragraph is re-parsed for every
    target. 7k–44k edges ⇒ 40k–250k synchronous parses without yielding. This starves BullMQ lock renewal → job marked
    stalled → re-delivered to the other replica → **duplicate concurrent plans** of the same site (races in `clusterSite`
    delete/insert, chunkPost, anchors). Cache noun phrases per paragraph id.
30. **H — Quadratic loops**: `edges.rows.find` inside the edge loop (`nlp.ts:444`); over-optimization guard recomputes
    `share()` with a full `filter` and a linear `find` per downgrade (`cannibalization.ts:60-80`) over pools of 40k+
    anchors; diversity guard likewise per group (`cannibalization.ts:126-150`). Keep running counts.
31. **H — Per-row DB round trips**: anchor UPDATE per edge with exception-driven retry (`nlp.ts:448-461`), paragraph_id
    UPDATE per edge (`nlp.ts:467-476`), topology insert per edge (`topologies.ts:127-135`), archive insert per edge,
    chunk upsert per paragraph (`chunk.ts:141-154`), GSC insert per row (`gsc.ts:146-160`), link-health upsert per link
    (`runLinkHealth.ts:468`) and per-post rollup, keyword-linker paragraph query per match, cluster member updates per cluster.
32. **H — `embedSite` is all-or-nothing and unbounded.** Loads every missing paragraph (300k on a big site) and all vectors
    (`number[]`, ~1 GB+) into memory, embeds sequentially in batches of 16 (~19k serial HTTP calls), and only persists at the
    end (`qdrantIndex.ts:95-118`) — any failure loses all work. Needs streaming batches with per-batch commit and bounded
    concurrency.
33. **H — Qdrant paragraph wipe on incremental re-embeds.** `qdrantIndex.ts:107` deletes all site paragraph points
    whenever *any* paragraph needs embedding, but chunkPost now preserves unchanged embeddings, so unchanged paragraphs vanish
    from `sf_paragraphs` permanently (the "exact parity" claim in CAPABILITY_AUDIT is false after the first incremental plan;
    harmless today only because nothing queries `sf_paragraphs`).
34. **M — Repeated HTML parsing**: each plan parses every post twice (chunk + graph), link health a third time with cheerio,
    `computeSiteMetrics → getComponents` a fourth (`metrics/history.ts:304`), verify parses each page ~6 times per attempt.
    Cache analysis by `content_hash`.
35. **M — Index shift churn**: chunkPost keys rows by `idx`, so inserting one paragraph at the top NULLs every following
    embedding and re-embeds the whole post (`chunk.ts:141-154`). Match by hash first.
36. **M — Sequential inject + verify**: per edge PUT + up to 3 fetches with 1.5 s/3 s backoff, 20 s timeouts
    (`inject.ts:107-239`). Group by post, one PUT per post, verify once.
37. **M — Unbounded growth**: full-HTML snapshot per edge, no retention; keywords never pruned; existing edges never pruned.
38. **M — k-means++ seeding is O(n·k²·d)** (`kmeans.rs:100-110`) — recomputes all distances to all centroids each round;
    keep a running min.

### Ghost issues / silent failures

39. **H — Rust engine silently disabled for the process lifetime** after one timeout or crash (`rust/client.ts:265,314`); a
    single 30 s timeout on a large page moves all subsequent parsing/graph/kmeans onto the event loop with only one warning.
40. **H — Nav provenance is dead code.** `link_health` extracts links from stored `content_html` only
    (`runLinkHealth.ts:312`), which never contains site navigation (REST body-only; sitemap mode strips nav), so
    `placement ∈ nav/header/footer/aside` practically never occurs and `is_nav_only`/contextual-orphan logic
    (`graph/engine.ts:210-231`) degrades to plain orphans.
41. **M — Settings › Models embed chains are ignored**: `providers/chain.ts::runEmbed` exists but `embedTexts` uses its own env
    chain (`embed/embedder.ts:83-109`).
42. **M — Mixed-rung vectors**: failover between remote (256-token window) and local (512) mid-run stores vectors from
    different effective inputs in one index unless `EMBED_MAX_CHARS` is set (default 0).
43. **M — Catch-all swallow** in anchor persistence (`nlp.ts:458`) hides any DB error, not just the unique collision;
    edges then get deleted by the blank-anchor safety net.
44. **M — No inbound cap per target**: reverse/bidirectional on a 400-member cluster sends hundreds of suggestions to one
    pillar; density only limits per source.
45. **L — Doc drift**: ARCHITECTURE.md says authority = source PageRank and graph = target centrality (code: authority = target,
    graph rewards LOW-PR targets), wink-nlp chunking (code: cheerio/Rust), and edge grouping per post on inject (not implemented).

---

## C. Accuracy improvements (evidence-based)

### Anchor selection
1. **Semantic phrase scoring (close G1, still open).** Embed the ≤40 candidate phrases per paragraph with the local MiniLM in
   Rust (cheap: tens of µs each in ONNX) and score `α·cos(phrase, target) + β·lexical` where lexical uses IDF weights.
   PLAN_v1.2.0 §5 rejected this only because of remote API cost; local Rust inference removes that constraint.
2. **Real keyphrases instead of unigrams** (#9): derive target descriptors from title/H1/slug n-grams (noun chunks), YAKE/RAKE
   on body, and GSC queries; pick `topKeyword` deterministically (score desc, then length desc, then lexical). This fixes
   exact/partial classification and makes the 40% exact guard meaningful.
3. **IDF / BM25 weighting** of target tokens across the site: on a niche site the niche noun ("bike") appears in every post and
   currently dominates overlap (`scorePhrase` uniform weights).
4. **Lemmatize** with the tagger's lemma output (lock/locks, review/reviews) before overlap; current `tokenSet` is exact-form.
5. **Ban one-word anchors** (fallback at `nlp.ts:226`) and anchors equal to the **source** post's own primary keyword
   (self-cannibalization), and block generation of an exact anchor already used for a different target
   (turn `getCannibalizationWarnings` into a generation-time constraint).
6. **Deliberate diversity**: when a target's pool is skewed, generate branded / partial / CTA variants explicitly (audit:
   7 branded vs 46k partial) instead of only downgrading to whatever alternates exist.
7. **Word-boundary, tag-spanning matching** in the splicer, skipping headings, captions, code, buttons, and paragraphs that
   already contain a link (or a link to the same target).
8. **Sentence-level placement**: split long blocks (lists are merged into one block, often > 256 tokens and truncated by
   MiniLM) into sentences for embedding; choose the sentence with max cosine and anchor within it; prefer earlier sentences
   and at most one new link per paragraph.

### Contextual relevance
9. **Two-stage retrieval** (LOOM-style, `sf_paragraphs` is already indexed but unused): post-level ANN for recall, then
   paragraph-level re-rank `max_p cos(p, target)`; use that as the semantic dimension instead of whole-post cosine of
   `title + 2000 chars` (which is truncated to ~1300 chars remotely).
10. **Calibrate the semantic threshold** (`SEMANTIC_MIN_SCORE 0.5`) per site from the cosine distribution (e.g. top decile),
    and require a minimum paragraph similarity before suggesting any pair.
11. **Fix depth roots** (homepage + nav targets) so the depth dimension carries signal; rescore all suggested edges every plan;
    replace site-wide availability with variance-based availability (a dim with near-constant value across candidates
    should be dropped/renormalized).
12. **Relative ladder thresholds** (CAPABILITY_AUDIT G-1 alternative): native at ≥ site p75 composite in addition to
    absolute thresholds.

### Silo linking
13. **Pillar selection by hub-ness**, not centroid proximity alone: combine closeness to centroid with inbound count, shallow
    URL, word count, and title generality; let operators pin.
14. **Exclude utility and archive pages** (privacy, contact, about, tag/category archives) from k-means; use cosine/spherical
    k-means with silhouette-driven k and an outlier bucket so off-topic posts are not forced to link to a pillar; require
    `cos(member, pillar) ≥ τ` for topology edges.
15. **Order serial/circle chains semantically** (greedy nearest-neighbour path over cosine) instead of UUID order
    (`topologies.ts:104`).
16. **Per-target inbound budget per pass** and anchor diversity per pillar; skip pairs already linked in either direction when
    the topology is bidirectional.
17. **Durable rejection memory**: never re-suggest a (source,target) pair that was rejected or is already linked (#6).

---

## D. Suggested Rust module map (for the rewrite)

| Rust module | Replaces | Notes |
|---|---|---|
| `crawl` | wpCrawl, sitemapCrawl, builder, ssrfGuard | raw content (`context=edit`), per-hop SSRF guard, pinned DNS resolver |
| `html` | chunk.ts + html.rs + extractLinks | one splitter, byte offsets, block-aware splicer |
| `embed` | embedder, qdrantIndex, reaper | ort/candle MiniLM, streaming batches, consistent 256-token truncation |
| `nlp` | anchor/nlp, classify | POS tagger, lemmas, cached per paragraph hash |
| `graph` | graph/engine + graph.rs | existing Rust core; fix roots, prune stale edges |
| `silo` | cluster, topologies, customClusters, drift | existing kmeans; fixed drift query |
| `score` | composite, weights, density | pure functions, golden tests from `composite.test.ts` |
| `insert` | insert, inject, verify, llm | per-post transactions, snapshot per post, validated LLM output |
| `providers` | providers/* | single chain for chat + embed |
| `health`, `metrics`, `rules`, `keywords` | as named | bulk SQL (COPY/unnest) everywhere |
