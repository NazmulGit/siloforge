// Golden fixture generator: pure functions copied verbatim (types stripped) from
// app/packages/engine/src/silo/cluster.ts and src/score/{composite,weights}.ts (v1.4.0).
import { writeFileSync } from 'node:fs';
const out = process.argv[2];

function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
function hashSeed(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); }
  return h >>> 0;
}
function sqDist(a, b) { let d = 0; for (let i = 0; i < a.length; i++) { const diff = a[i] - b[i]; d += diff * diff; } return d; }
const MAX_ITERS = 50;
function kmeans(vectors, k, seed) {
  const n = vectors.length;
  const rng = mulberry32(hashSeed(seed));
  const dim = vectors[0].length;
  const centroids = [vectors[Math.floor(rng() * n)].slice()];
  while (centroids.length < k) {
    const dists = vectors.map((v) => Math.min(...centroids.map((c) => sqDist(v, c))));
    const total = dists.reduce((a, b) => a + b, 0);
    if (total === 0) { centroids.push(vectors[Math.floor(rng() * n)].slice()); continue; }
    let r = rng() * total; let idx = 0;
    for (let i = 0; i < n; i++) { r -= dists[i]; if (r <= 0) { idx = i; break; } }
    centroids.push(vectors[idx].slice());
  }
  let assignments = new Array(n).fill(0);
  for (let iter = 0; iter < MAX_ITERS; iter++) {
    let changed = false;
    const next = vectors.map((v, i) => {
      let best = 0; let bestD = Infinity;
      for (let c = 0; c < k; c++) { const d = sqDist(v, centroids[c]); if (d < bestD) { bestD = d; best = c; } }
      if (best !== assignments[i]) changed = true;
      return best;
    });
    assignments = next;
    if (!changed && iter > 0) break;
    const sums = Array.from({ length: k }, () => new Array(dim).fill(0));
    const counts = new Array(k).fill(0);
    for (let i = 0; i < n; i++) { counts[assignments[i]]++; const v = vectors[i]; const s = sums[assignments[i]]; for (let d = 0; d < dim; d++) s[d] += v[d]; }
    for (let c = 0; c < k; c++) { if (counts[c] === 0) continue; for (let d = 0; d < dim; d++) centroids[c][d] = sums[c][d] / counts[c]; }
  }
  return { assignments, centroids };
}
function defaultK(n) { return Math.max(2, Math.min(25, Math.round(Math.sqrt(n / 2)))); }

const WEIGHTS = { semantic: 0.22, orphan: 0.08, depth: 0.06, tier: 0.06, cluster: 0.06, velocity: 0.04, graph: 0.10, money: 0.10, gsc: 0.08, authority: 0.10, placement: 0.10 };
const clamp01 = (v) => Math.max(0, Math.min(1, v));
function computeBreakdown(i) {
  const semantic = clamp01(i.cosine);
  const incoming = i.targetIncoming;
  const orphan = incoming === 0 ? 1.0 : incoming < 3 ? 0.5 : incoming < 5 ? 0.2 : 0.0;
  const d = i.targetClickDepth;
  const depth = d === null || d < 0 ? 1.0 : d > 4 ? 1.0 : d > 3 ? 0.7 : d === 3 ? 0.3 : 0.0;
  const tier = i.targetTier < i.sourceTier ? 1.0 : i.targetTier === i.sourceTier ? 0.3 : 0.1;
  let cluster = 0.0;
  if (i.targetIsPillar && i.sourceClusterId !== null && i.sourceClusterId === i.targetClusterId) cluster = 1.0;
  else if (i.sourceClusterId !== null && i.sourceClusterId === i.targetClusterId) cluster = 0.8;
  else if (i.targetClusterId !== null) cluster = 0.2;
  const ageDays = Math.max(1, i.targetAgeDays ?? 90);
  const ageMonths = Math.max(1, ageDays / 30);
  const linksPerMonth = incoming / ageMonths;
  let velocity;
  if (incoming === 0) velocity = 0.8;
  else if (ageDays > 180 && linksPerMonth < 0.5) velocity = 0.7;
  else if (ageDays > 90 && linksPerMonth < 1.0) velocity = 0.5;
  else if (linksPerMonth < 1.0) velocity = 0.2;
  else velocity = 0.0;
  let graph = 0.0;
  if (i.targetIsOrphan) graph += 0.4;
  if (i.avgPagerank > 0 && i.targetPagerank < i.avgPagerank * 0.5) graph += 0.3;
  if (i.targetIsDeadEnd && i.targetPagerank > i.avgPagerank) graph -= 0.15;
  if (i.sameComponent) graph += 0.15;
  if (i.avgPagerank > 0 && i.sourcePagerank > i.avgPagerank * 2) graph += 0.1;
  graph = clamp01(graph);
  let money = 0.0;
  if (i.targetIsMoney || i.sourceIsMoney) {
    if (i.targetIsMoney) {
      money += 0.5; money += (i.moneyPriority ?? 1) * 0.05;
      const goal = i.moneyLinksGoal ?? 10; const deficit = Math.max(0, goal - incoming);
      if (deficit > 0) money += Math.min(0.25, deficit * 0.05); else money -= 0.2;
    }
    if (i.sourceIsMoney && !i.targetIsMoney) money -= 0.3;
    money = clamp01(money);
  }
  const gsc = !i.gscConnected || i.strikingCount === 0 ? 0.0 : clamp01(0.5 + 0.25 * (i.strikingCount - 1));
  let authority = 0.0;
  if (i.targetClusterId !== null) authority += Math.min(0.4, i.clusterInboundLinks * 0.1);
  if (i.avgPagerank > 0) { const ratio = i.targetPagerank / i.avgPagerank; if (ratio >= 3.0) authority += 0.3; else if (ratio >= 1.5) authority += 0.2; else if (ratio >= 1.0) authority += 0.1; }
  const kw = i.targetKeywordCount;
  if (kw >= 5) authority += 0.3; else if (kw >= 3) authority += 0.2; else if (kw >= 1) authority += 0.1;
  authority = clamp01(authority);
  let placement = 0.0;
  if (i.bestParagraph !== null && i.bestParagraph > 0 && i.paragraphSim > 0.3) {
    const totalParas = Math.max(0, i.sourceWordCount || 500) / 100;
    const ratio = totalParas > 0 ? i.bestParagraph / Math.max(1, totalParas) : 0.5;
    if (ratio <= 0.33) placement = Math.min(1, i.paragraphSim * 1.5);
    else if (ratio <= 0.66) placement = Math.min(1, i.paragraphSim * 1.0);
    else placement = Math.min(1, i.paragraphSim * 0.5);
  }
  return { semantic, orphan, depth, tier, cluster, velocity, graph, money, gsc, authority, placement, weights: { ...WEIGHTS } };
}
function effectiveWeights(av) {
  const excluded = ['gsc', 'money', 'tier'].filter((d) => !av[d]);
  const o = { ...WEIGHTS };
  if (excluded.length === 0) return o;
  const removed = excluded.reduce((s, d) => s + WEIGHTS[d], 0);
  const scale = 1 / (1 - removed);
  for (const key of Object.keys(o)) o[key] = excluded.includes(key) ? 0 : Math.round(o[key] * scale * 10000) / 10000;
  const sum = Object.values(o).reduce((a, b) => a + b, 0);
  o.semantic = Math.round((o.semantic + (1 - sum)) * 10000) / 10000;
  return o;
}
function compositeScore(b, av) {
  const w = effectiveWeights(av);
  const s = b.semantic * w.semantic + b.orphan * w.orphan + b.depth * w.depth + b.tier * w.tier + b.cluster * w.cluster +
    b.velocity * w.velocity + b.graph * w.graph + b.money * w.money + b.gsc * w.gsc + b.authority * w.authority + b.placement * w.placement;
  return Math.round(s * 10000) / 10000;
}

// ---- kmeans fixtures ----
const g = mulberry32(12345);
const gauss = () => { const u = Math.max(1e-12, g()); const v = g(); return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * v); };
const cases = [];
function blobs(n, dim, centers, spread) {
  const cs = Array.from({ length: centers }, () => Array.from({ length: dim }, () => gauss() * 3));
  return Array.from({ length: n }, (_, i) => cs[i % centers].map((c) => c + gauss() * spread));
}
function unit(vs) { return vs.map((v) => { const n = Math.sqrt(v.reduce((a, b) => a + b * b, 0)); return v.map((x) => x / n); }); }
const specs = [
  ['tiny', [[0, 0], [0, 1], [9, 9], [10, 9]], 2, 'site-123'],
  ['blobs', blobs(30, 3, 3, 0.5), 3, 'site-blobs'],
  ['unit16', unit(blobs(80, 16, 6, 1.5)), defaultK(80), '0b9c1f9e-4f7a-4c55-9d2a-3c4b5a6d7e8f'],
  ['noisy', unit(blobs(120, 8, 4, 4)), 7, 'site-ünïcødé-\u{1F600}'],
  ['dupes', [[1, 0], [1, 0], [1, 0], [1, 0], [0, 1]], 3, 'dupes'],
];
for (const [name, vectors, k, seed] of specs) {
  const r = kmeans(vectors, k, seed);
  cases.push({ name, vectors, k, seed, assignments: r.assignments, centroids: r.centroids });
}
const hash = [['\u{1F600}', hashSeed('\u{1F600}')], ['site-123', hashSeed('site-123')], ['', hashSeed('')]];
writeFileSync(`${out}/cluster/fixtures/kmeans_parity.json`, JSON.stringify({
  hash,
  defaultK: [2, 8, 50, 100, 1000, 2000, 5000].map((n) => [n, defaultK(n)]),
  cases,
}));

// ---- composite fixtures ----
const r2 = mulberry32(777);
const pick = (arr) => arr[Math.floor(r2() * arr.length)];
const rows = [];
for (let n = 0; n < 400; n++) {
  const avg = pick([0, 0.001, 0.01, 1]);
  const inp = {
    cosine: pick([-0.2, 0, r2(), r2(), 1.3]),
    targetIncoming: pick([0, 1, 2, 3, 4, 5, 9, 10, 12, 40]),
    targetClickDepth: pick([null, -1, 0, 1, 2, 3, 4, 5, 9]),
    sourceTier: pick([0, 1, 2, 3]), targetTier: pick([0, 1, 2, 3]),
    sourceClusterId: pick([null, 'c1', 'c2']), targetClusterId: pick([null, 'c1', 'c2']),
    targetIsPillar: r2() < 0.3,
    targetAgeDays: pick([null, 1, 20, 45, 91, 120, 181, 400, 2000]),
    targetIsOrphan: r2() < 0.3, targetIsDeadEnd: r2() < 0.3,
    targetPagerank: avg * pick([0, 0.3, 0.6, 1, 1.2, 1.6, 3.5]),
    sourcePagerank: avg * pick([0, 0.5, 1, 2.5]),
    avgPagerank: avg, sameComponent: r2() < 0.5,
    targetIsMoney: r2() < 0.2, sourceIsMoney: r2() < 0.2,
    strikingCount: pick([0, 0, 1, 2, 3, 5]), gscConnected: r2() < 0.5,
    clusterInboundLinks: pick([0, 1, 3, 7]), targetKeywordCount: pick([0, 1, 3, 5, 8]),
    bestParagraph: pick([null, 0, 1, 2, 4, 7, 12]), paragraphSim: pick([0, 0.2, 0.3, r2(), 0.95]),
    sourceWordCount: pick([0, 120, 500, 900, 2400]),
  };
  const av = { gsc: r2() < 0.5, money: r2() < 0.5, tier: r2() < 0.5 };
  const b = computeBreakdown(inp);
  rows.push({ input: inp, availability: av, breakdown: { ...b, weights: effectiveWeights(av) }, score: compositeScore(b, av) });
}
const weights = [];
for (const gsc of [true, false]) for (const money of [true, false]) for (const tier of [true, false]) {
  weights.push({ availability: { gsc, money, tier }, weights: effectiveWeights({ gsc, money, tier }) });
}
writeFileSync(`${out}/score/fixtures/composite_golden.json`, JSON.stringify({ weights, rows }));
console.log('ok', cases.length, rows.length);
