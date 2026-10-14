//! Dense-vector math and brute-force nearest-neighbour search (replaces Qdrant, spec §2.3).
//!
//! * Plain f32 kernels written as fixed-width chunked loops so LLVM auto-vectorises them on the
//!   baseline x86-64 target (no `unsafe`, no target-cpu flags; the binary must run on any PC).
//! * Top-k uses a bounded heap; ties break by lower row index so results are deterministic.
//! * [`batch_top_k`] tiles queries x rows for cache reuse and runs query blocks in parallel on
//!   rayon's *current* pool — call it inside [`crate::pool::install`].
//! * int8 symmetric quantisation ([`QuantizedMatrix`]) gives a 4x smaller candidate scan; results
//!   are always re-ranked with exact f32 dot products.
//! * [`VectorIndex`] = one flat contiguous `Vec<f32>` (row-major, L2-normalised rows) + ids.
//!
//! Hostile input never panics: mismatched dimensions, NaN/inf, zero vectors, `k == 0` or
//! `k > n` yield empty or truncated results (or a [`VectorError`] where a `Result` is returned).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use rayon::prelude::*;

/// all-MiniLM-L6-v2 dimension (spec §2.1).
pub const EMBED_DIM: usize = 384;
/// Largest accepted dimension.
pub const MAX_DIM: usize = 4096;
/// Largest number of f32 values one index / matrix may hold (1 GiB of f32).
pub const MAX_INDEX_VALUES: usize = 256 * 1024 * 1024;
/// Largest `k` honoured by any search (larger values are clamped).
pub const MAX_K: usize = 10_000;
/// Norm below which a vector counts as zero (same threshold as v1 `toUnitVector`).
pub const MIN_NORM: f32 = 1e-8;

const LANES: usize = 16;
const QUERY_BLOCK: usize = 16;
const ROW_TILE: usize = 256;

/// Errors from fallible vector operations.
#[derive(Debug, Clone, PartialEq)]
pub enum VectorError {
    /// Dimension is 0 or above [`MAX_DIM`].
    BadDim(usize),
    /// Vector length differs from the index dimension.
    DimMismatch { expected: usize, got: usize },
    /// A value is NaN or infinite.
    NonFinite,
    /// Norm is (close to) zero, so the vector has no direction.
    ZeroNorm,
    /// The index would exceed [`MAX_INDEX_VALUES`].
    TooLarge,
    /// Id is empty or longer than 512 bytes.
    BadId,
}

impl std::fmt::Display for VectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadDim(d) => write!(f, "vector dimension {d} is out of range 1..={MAX_DIM}"),
            Self::DimMismatch { expected, got } => {
                write!(f, "vector has {got} dimensions, expected {expected}")
            }
            Self::NonFinite => write!(f, "vector contains NaN or infinite values"),
            Self::ZeroNorm => write!(f, "vector norm is zero"),
            Self::TooLarge => write!(f, "vector index exceeds {MAX_INDEX_VALUES} values"),
            Self::BadId => write!(f, "vector id must be 1..=512 bytes"),
        }
    }
}

impl std::error::Error for VectorError {}

// ---------------------------------------------------------------------------------------------
// Kernels
// ---------------------------------------------------------------------------------------------

/// Lanes of the f32 kernels. `dot` and `dot4` share the exact same summation order so single
/// and batched searches return bit-identical scores.
const F32_LANES: usize = 8;

#[inline]
fn reduce8(a: &[f32; F32_LANES]) -> f32 {
    ((a[0] + a[4]) + (a[1] + a[5])) + ((a[2] + a[6]) + (a[3] + a[7]))
}

/// Dot product. Returns 0.0 when the lengths differ.
#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut acc = [0f32; F32_LANES];
    let (ca, ra) = a.as_chunks::<F32_LANES>();
    let (cb, rb) = b.as_chunks::<F32_LANES>();
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..F32_LANES {
            acc[i] += x[i] * y[i];
        }
    }
    let mut s = reduce8(&acc);
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

/// Dot products of `q` with four rows at once (query loads reused). Same per-row summation
/// order as [`dot`]. Rows of a different length than `q` score 0.
#[inline]
fn dot4(q: &[f32], r0: &[f32], r1: &[f32], r2: &[f32], r3: &[f32]) -> [f32; 4] {
    let n = q.len();
    if r0.len() != n || r1.len() != n || r2.len() != n || r3.len() != n {
        return [dot(q, r0), dot(q, r1), dot(q, r2), dot(q, r3)];
    }
    let mut a0 = [0f32; F32_LANES];
    let mut a1 = [0f32; F32_LANES];
    let mut a2 = [0f32; F32_LANES];
    let mut a3 = [0f32; F32_LANES];
    let it = q
        .as_chunks::<F32_LANES>()
        .0
        .iter()
        .zip(r0.as_chunks::<F32_LANES>().0)
        .zip(r1.as_chunks::<F32_LANES>().0)
        .zip(r2.as_chunks::<F32_LANES>().0)
        .zip(r3.as_chunks::<F32_LANES>().0);
    for ((((qc, c0), c1), c2), c3) in it {
        for i in 0..F32_LANES {
            a0[i] += qc[i] * c0[i];
            a1[i] += qc[i] * c1[i];
            a2[i] += qc[i] * c2[i];
            a3[i] += qc[i] * c3[i];
        }
    }
    let tail = n / F32_LANES * F32_LANES;
    let mut out = [reduce8(&a0), reduce8(&a1), reduce8(&a2), reduce8(&a3)];
    for (o, r) in out.iter_mut().zip([r0, r1, r2, r3]) {
        for (x, y) in q
            .get(tail..)
            .unwrap_or(&[])
            .iter()
            .zip(r.get(tail..).unwrap_or(&[]))
        {
            *o += x * y;
        }
    }
    out
}

/// Euclidean norm.
#[inline]
pub fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

/// Cosine similarity in [-1, 1]. 0.0 for mismatched lengths, empty, zero or non-finite input.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (na, nb) = (norm(a), norm(b));
    if !na.is_finite() || !nb.is_finite() || na <= MIN_NORM || nb <= MIN_NORM {
        return 0.0;
    }
    let c = dot(a, b) / (na * nb);
    if c.is_finite() {
        c.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// L2-normalise in place. Fails (leaving `v` untouched) on NaN/inf or a zero vector.
pub fn normalize(v: &mut [f32]) -> Result<(), VectorError> {
    if v.iter().any(|x| !x.is_finite()) {
        return Err(VectorError::NonFinite);
    }
    let n = norm(v);
    if !n.is_finite() || n <= MIN_NORM {
        return Err(VectorError::ZeroNorm);
    }
    for x in v.iter_mut() {
        *x /= n;
    }
    Ok(())
}

/// Normalised copy of `v`, checked against `dim`.
pub fn normalized(v: &[f32], dim: usize) -> Result<Vec<f32>, VectorError> {
    if dim == 0 || dim > MAX_DIM {
        return Err(VectorError::BadDim(dim));
    }
    if v.len() != dim {
        return Err(VectorError::DimMismatch {
            expected: dim,
            got: v.len(),
        });
    }
    let mut out = v.to_vec();
    normalize(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Top-k
// ---------------------------------------------------------------------------------------------

/// One search result: row index into the matrix / index, and its score (dot product).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub index: usize,
    pub score: f32,
}

/// Heap entry ordered so that the *worst* hit is the max (top) of a `BinaryHeap`.
#[derive(Debug, Clone, Copy)]
struct Worst(Hit);

impl PartialEq for Worst {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Worst {}
impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        // lower score = worse = greater; equal score: higher index = worse
        other
            .0
            .score
            .total_cmp(&self.0.score)
            .then(self.0.index.cmp(&other.0.index))
    }
}

/// Bounded best-k collector (score desc, index asc). NaN scores are ignored.
#[derive(Debug, Clone)]
pub struct TopK {
    k: usize,
    heap: BinaryHeap<Worst>,
}

impl TopK {
    /// Collector for at most `k` (clamped to [`MAX_K`]) hits.
    pub fn new(k: usize) -> Self {
        let k = k.min(MAX_K);
        Self {
            k,
            heap: BinaryHeap::with_capacity(k.saturating_add(1)),
        }
    }

    /// Offer one candidate.
    #[inline]
    pub fn push(&mut self, index: usize, score: f32) {
        if self.k == 0 || score.is_nan() {
            return;
        }
        let item = Worst(Hit { index, score });
        if self.heap.len() < self.k {
            self.heap.push(item);
        } else if let Some(top) = self.heap.peek()
            && item < *top
        {
            self.heap.pop();
            self.heap.push(item);
        }
    }

    /// Current worst kept score once full (None while not full); lets callers prune early.
    pub fn threshold(&self) -> Option<f32> {
        if self.heap.len() < self.k {
            None
        } else {
            self.heap.peek().map(|w| w.0.score)
        }
    }

    /// Hits sorted best first.
    pub fn into_sorted(self) -> Vec<Hit> {
        let mut v: Vec<Worst> = self.heap.into_vec();
        v.sort_unstable();
        v.into_iter().map(|w| w.0).collect()
    }
}

fn matrix_ok(matrix: &[f32], dim: usize) -> bool {
    dim > 0 && dim <= MAX_DIM && matrix.len().is_multiple_of(dim)
}

/// Best `k` rows of a flat row-major `matrix` by dot product with `query`.
/// Rows are assumed normalised when cosine is wanted. Bad shapes return an empty Vec.
pub fn top_k(query: &[f32], matrix: &[f32], dim: usize, k: usize) -> Vec<Hit> {
    top_k_filtered(query, matrix, dim, k, |_| true)
}

/// [`top_k`] over rows for which `keep(row_index)` is true.
pub fn top_k_filtered(
    query: &[f32],
    matrix: &[f32],
    dim: usize,
    k: usize,
    keep: impl Fn(usize) -> bool,
) -> Vec<Hit> {
    if !matrix_ok(matrix, dim) || query.len() != dim || query.iter().any(|x| !x.is_finite()) {
        return Vec::new();
    }
    let mut top = TopK::new(k);
    for (i, row) in matrix.chunks_exact(dim).enumerate() {
        if keep(i) {
            top.push(i, dot(query, row));
        }
    }
    top.into_sorted()
}

/// Top-k for many queries at once (`queries` is flat row-major, `dim` wide). Parallel over
/// query blocks on rayon's current pool, tiled for cache reuse. Result `i` belongs to query `i`;
/// a query with NaN/inf values gets an empty list.
pub fn batch_top_k(queries: &[f32], matrix: &[f32], dim: usize, k: usize) -> Vec<Vec<Hit>> {
    batch_top_k_filtered(queries, matrix, dim, k, |_, _| true)
}

/// [`batch_top_k`] keeping only pairs where `keep(query_index, row_index)` is true
/// (e.g. to drop the query's own row).
pub fn batch_top_k_filtered<F>(
    queries: &[f32],
    matrix: &[f32],
    dim: usize,
    k: usize,
    keep: F,
) -> Vec<Vec<Hit>>
where
    F: Fn(usize, usize) -> bool + Sync,
{
    if !matrix_ok(matrix, dim) || !matrix_ok(queries, dim) {
        return Vec::new();
    }
    let blocks: Vec<Vec<Vec<Hit>>> = queries
        .par_chunks(QUERY_BLOCK * dim)
        .enumerate()
        .map(|(b, block)| {
            let qbase = b * QUERY_BLOCK;
            let qs: Vec<Option<&[f32]>> = block
                .chunks_exact(dim)
                .map(|q| q.iter().all(|x| x.is_finite()).then_some(q))
                .collect();
            let mut tops: Vec<TopK> = qs.iter().map(|_| TopK::new(k)).collect();
            for (t, tile) in matrix.chunks(ROW_TILE * dim).enumerate() {
                let rbase = t * ROW_TILE;
                for ((qi, q), top) in qs.iter().enumerate().zip(tops.iter_mut()) {
                    let Some(q) = q else { continue };
                    let quads = tile.chunks_exact(4 * dim);
                    let rest = quads.remainder();
                    for (gi, quad) in quads.enumerate() {
                        let mut it = quad.chunks_exact(dim);
                        let (Some(r0), Some(r1), Some(r2), Some(r3)) =
                            (it.next(), it.next(), it.next(), it.next())
                        else {
                            continue;
                        };
                        let s = dot4(q, r0, r1, r2, r3);
                        let base = rbase + gi * 4;
                        for (j, sc) in s.iter().enumerate() {
                            if keep(qbase + qi, base + j) {
                                top.push(base + j, *sc);
                            }
                        }
                    }
                    let rest_base = rbase + (tile.len() - rest.len()) / dim;
                    for (ri, row) in rest.chunks_exact(dim).enumerate() {
                        let idx = rest_base + ri;
                        if keep(qbase + qi, idx) {
                            top.push(idx, dot(q, row));
                        }
                    }
                }
            }
            tops.into_iter().map(TopK::into_sorted).collect()
        })
        .collect();
    blocks.into_iter().flatten().collect()
}

// ---------------------------------------------------------------------------------------------
// int8 quantisation
// ---------------------------------------------------------------------------------------------

/// Symmetric per-vector int8 quantisation: `x ≈ code * scale`, `scale = max|x| / 127`.
/// Non-finite values quantise to 0; an all-zero vector gets scale 0.
pub fn quantize_vector(v: &[f32]) -> (Vec<i8>, f32) {
    let max = v
        .iter()
        .filter(|x| x.is_finite())
        .fold(0f32, |m, x| m.max(x.abs()));
    if max <= 0.0 || !max.is_finite() {
        return (vec![0; v.len()], 0.0);
    }
    let scale = max / 127.0;
    let codes = v
        .iter()
        .map(|x| {
            if x.is_finite() {
                // value is within [-127, 127] by construction
                (x / scale).round().clamp(-127.0, 127.0) as i8
            } else {
                0
            }
        })
        .collect();
    (codes, scale)
}

/// Integer dot product of two code vectors (0 when lengths differ). Cannot overflow for
/// `len <= MAX_DIM` (127² · 4096 < 2³¹).
#[inline]
pub fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    if a.len() != b.len() {
        return 0;
    }
    let mut acc = [0i32; LANES];
    let (ca, ra) = a.as_chunks::<LANES>();
    let (cb, rb) = b.as_chunks::<LANES>();
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..LANES {
            acc[i] += i32::from(x[i]) * i32::from(y[i]);
        }
    }
    let mut s: i32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += i32::from(*x) * i32::from(*y);
    }
    s
}

/// int8 copy of a row-major matrix: `n * dim` codes + one scale per row.
#[derive(Debug, Clone, Default)]
pub struct QuantizedMatrix {
    dim: usize,
    codes: Vec<i8>,
    scales: Vec<f32>,
}

impl QuantizedMatrix {
    /// Quantise every row of `matrix`.
    pub fn from_f32(matrix: &[f32], dim: usize) -> Result<Self, VectorError> {
        if dim == 0 || dim > MAX_DIM {
            return Err(VectorError::BadDim(dim));
        }
        if !matrix.len().is_multiple_of(dim) {
            return Err(VectorError::DimMismatch {
                expected: dim,
                got: matrix.len() % dim,
            });
        }
        if matrix.len() > MAX_INDEX_VALUES {
            return Err(VectorError::TooLarge);
        }
        let rows: Vec<(Vec<i8>, f32)> = matrix.par_chunks(dim).map(quantize_vector).collect();
        let mut codes = Vec::with_capacity(matrix.len());
        let mut scales = Vec::with_capacity(rows.len());
        for (c, s) in rows {
            codes.extend_from_slice(&c);
            scales.push(s);
        }
        Ok(Self { dim, codes, scales })
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.scales.len()
    }

    /// True when there are no rows.
    pub fn is_empty(&self) -> bool {
        self.scales.is_empty()
    }

    /// Row dimension.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Bytes used by codes + scales.
    pub fn memory_bytes(&self) -> usize {
        self.codes.len() + self.scales.len() * std::mem::size_of::<f32>()
    }

    /// Approximate top-`n` rows for a query already quantised with [`quantize_vector`].
    pub fn approx_top(&self, qcodes: &[i8], qscale: f32, n: usize) -> Vec<Hit> {
        if qcodes.len() != self.dim || self.dim == 0 {
            return Vec::new();
        }
        let mut top = TopK::new(n);
        for (i, (row, s)) in self
            .codes
            .chunks_exact(self.dim)
            .zip(&self.scales)
            .enumerate()
        {
            top.push(i, dot_i8(qcodes, row) as f32 * qscale * s);
        }
        top.into_sorted()
    }
}

/// Two-stage search: int8 scan for `k * oversample` candidates, then exact f32 re-rank.
/// `matrix` must be the f32 source of `quant`. Returns at most `k` hits.
pub fn quantized_top_k(
    query: &[f32],
    matrix: &[f32],
    quant: &QuantizedMatrix,
    k: usize,
    oversample: usize,
) -> Vec<Hit> {
    let dim = quant.dim;
    if query.len() != dim
        || !matrix_ok(matrix, dim)
        || matrix.len() / dim.max(1) != quant.len()
        || query.iter().any(|x| !x.is_finite())
    {
        return Vec::new();
    }
    let k = k.min(MAX_K);
    let pool_size = k.saturating_mul(oversample.max(1)).max(k).min(MAX_K);
    let (qc, qs) = quantize_vector(query);
    let candidates = quant.approx_top(&qc, qs, pool_size);
    let mut top = TopK::new(k);
    for c in candidates {
        if let Some(row) = matrix.get(c.index * dim..(c.index + 1) * dim) {
            top.push(c.index, dot(query, row));
        }
    }
    top.into_sorted()
}

// ---------------------------------------------------------------------------------------------
// VectorIndex
// ---------------------------------------------------------------------------------------------

/// Flat in-memory vector index: contiguous normalised rows + parallel id list.
/// Scores are cosine similarities (rows and queries are L2-normalised).
#[derive(Debug, Clone)]
pub struct VectorIndex {
    dim: usize,
    data: Vec<f32>,
    ids: Vec<String>,
    positions: HashMap<String, usize>,
    quant: Option<QuantizedMatrix>,
}

/// Bytes an index of `n` vectors of `dim` with average id length `avg_id_len` needs
/// (f32 rows + ids + id map, plus int8 codes when `quantized`). Use it for cache budgets.
pub fn estimate_memory_bytes(n: usize, dim: usize, avg_id_len: usize, quantized: bool) -> usize {
    let f32_rows = n.saturating_mul(dim).saturating_mul(4);
    // String header + heap bytes, twice (ids Vec + map key) + map overhead (~16 B/entry)
    let per_id = (std::mem::size_of::<String>() + avg_id_len)
        .saturating_mul(2)
        .saturating_add(16 + std::mem::size_of::<usize>());
    let ids = n.saturating_mul(per_id);
    let q = if quantized {
        n.saturating_mul(dim).saturating_add(n.saturating_mul(4))
    } else {
        0
    };
    f32_rows.saturating_add(ids).saturating_add(q)
}

impl VectorIndex {
    /// Empty index for vectors of `dim`.
    pub fn new(dim: usize) -> Result<Self, VectorError> {
        if dim == 0 || dim > MAX_DIM {
            return Err(VectorError::BadDim(dim));
        }
        Ok(Self {
            dim,
            data: Vec::new(),
            ids: Vec::new(),
            positions: HashMap::new(),
            quant: None,
        })
    }

    /// Empty index with room for `n` vectors (no reallocation while loading a known row count;
    /// a growing `Vec` can otherwise hold up to 2x the f32 data it needs).
    pub fn with_capacity(dim: usize, n: usize) -> Result<Self, VectorError> {
        let mut idx = Self::new(dim)?;
        let n = n.min(MAX_INDEX_VALUES / dim);
        idx.data.reserve_exact(n.saturating_mul(dim));
        idx.ids.reserve_exact(n);
        idx.positions.reserve(n);
        Ok(idx)
    }

    /// Release spare capacity (call after loading).
    pub fn shrink_to_fit(&mut self) {
        self.data.shrink_to_fit();
        self.ids.shrink_to_fit();
        self.positions.shrink_to_fit();
    }

    /// Build from `(id, vector)` pairs. Invalid vectors are skipped and reported with their id;
    /// a duplicate id replaces the earlier vector.
    pub fn build<I>(dim: usize, items: I) -> Result<(Self, Vec<(String, VectorError)>), VectorError>
    where
        I: IntoIterator<Item = (String, Vec<f32>)>,
    {
        let mut idx = Self::new(dim)?;
        let mut rejected = Vec::new();
        for (id, v) in items {
            match idx.insert(&id, &v) {
                Ok(_) => {}
                Err(VectorError::TooLarge) => return Err(VectorError::TooLarge),
                Err(e) => rejected.push((id, e)),
            }
        }
        Ok((idx, rejected))
    }

    /// Insert or replace one vector (stored normalised). Returns its row index.
    /// Invalidates the int8 copy (call [`Self::quantize`] again).
    pub fn insert(&mut self, id: &str, v: &[f32]) -> Result<usize, VectorError> {
        if id.is_empty() || id.len() > 512 {
            return Err(VectorError::BadId);
        }
        let row = normalized(v, self.dim)?;
        self.quant = None;
        if let Some(&pos) = self.positions.get(id) {
            if let Some(slot) = self.data.get_mut(pos * self.dim..(pos + 1) * self.dim) {
                slot.copy_from_slice(&row);
            }
            return Ok(pos);
        }
        if self.data.len().saturating_add(self.dim) > MAX_INDEX_VALUES {
            return Err(VectorError::TooLarge);
        }
        let pos = self.ids.len();
        self.data.extend_from_slice(&row);
        self.ids.push(id.to_string());
        self.positions.insert(id.to_string(), pos);
        Ok(pos)
    }

    /// Number of vectors.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Vector dimension.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// All ids in row order.
    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// Id of a row.
    pub fn id(&self, index: usize) -> Option<&str> {
        self.ids.get(index).map(String::as_str)
    }

    /// Row of an id.
    pub fn position(&self, id: &str) -> Option<usize> {
        self.positions.get(id).copied()
    }

    /// Stored (normalised) vector of a row.
    pub fn vector(&self, index: usize) -> Option<&[f32]> {
        self.data.get(index * self.dim..(index + 1) * self.dim)
    }

    /// The flat row-major matrix.
    pub fn matrix(&self) -> &[f32] {
        &self.data
    }

    /// Map hits to `(id, score)`.
    pub fn resolve<'a>(&'a self, hits: &[Hit]) -> Vec<(&'a str, f32)> {
        hits.iter()
            .filter_map(|h| self.id(h.index).map(|id| (id, h.score)))
            .collect()
    }

    /// Exact cosine top-k. Invalid queries return an empty Vec.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<Hit> {
        self.search_filtered(query, k, |_, _| true)
    }

    /// Exact cosine top-k over rows where `keep(row_index, id)` is true.
    pub fn search_filtered(
        &self,
        query: &[f32],
        k: usize,
        keep: impl Fn(usize, &str) -> bool,
    ) -> Vec<Hit> {
        let Ok(q) = normalized(query, self.dim) else {
            return Vec::new();
        };
        top_k_filtered(&q, &self.data, self.dim, k, |i| {
            self.ids.get(i).is_some_and(|id| keep(i, id))
        })
    }

    /// Exact top-k for many queries in parallel (run inside [`crate::pool::install`]).
    /// Invalid queries get an empty list.
    pub fn search_batch(&self, queries: &[Vec<f32>], k: usize) -> Vec<Vec<Hit>> {
        let (flat, valid) = self.flatten_queries(queries);
        let res = batch_top_k(&flat, &self.data, self.dim, k);
        Self::spread(res, &valid, queries.len())
    }

    /// For each id, its `k` nearest *other* rows (v1 `findSimilarPostsBatch`: self dropped).
    /// Unknown ids get an empty list.
    pub fn similar_batch(&self, ids: &[&str], k: usize) -> Vec<Vec<Hit>> {
        let rows: Vec<Option<usize>> = ids.iter().map(|id| self.position(id)).collect();
        let known: Vec<usize> = rows.iter().flatten().copied().collect();
        let mut flat = Vec::with_capacity(known.len() * self.dim);
        for &r in &known {
            if let Some(v) = self.vector(r) {
                flat.extend_from_slice(v);
            }
        }
        let res = batch_top_k_filtered(&flat, &self.data, self.dim, k, |qi, ri| {
            known.get(qi).is_none_or(|&self_row| self_row != ri)
        });
        let mut it = res.into_iter();
        rows.iter()
            .map(|r| match r {
                Some(_) => it.next().unwrap_or_default(),
                None => Vec::new(),
            })
            .collect()
    }

    /// Build (or rebuild) the int8 copy used by [`Self::search_quantized`].
    pub fn quantize(&mut self) -> Result<(), VectorError> {
        self.quant = Some(QuantizedMatrix::from_f32(&self.data, self.dim)?);
        Ok(())
    }

    /// True when an int8 copy exists and is current.
    pub fn is_quantized(&self) -> bool {
        self.quant.is_some()
    }

    /// int8 candidate scan of `k * oversample` rows + exact re-rank. Falls back to exact
    /// search when [`Self::quantize`] has not been called.
    pub fn search_quantized(&self, query: &[f32], k: usize, oversample: usize) -> Vec<Hit> {
        let Some(quant) = &self.quant else {
            return self.search(query, k);
        };
        let Ok(q) = normalized(query, self.dim) else {
            return Vec::new();
        };
        quantized_top_k(&q, &self.data, quant, k, oversample)
    }

    /// Approximate heap bytes held by this index.
    pub fn memory_bytes(&self) -> usize {
        let ids: usize = self.ids.iter().map(|s| s.capacity()).sum();
        self.data.capacity() * 4
            + ids * 2
            + self.ids.len()
                * (2 * std::mem::size_of::<String>() + 16 + std::mem::size_of::<usize>())
            + self.quant.as_ref().map_or(0, QuantizedMatrix::memory_bytes)
    }

    fn flatten_queries(&self, queries: &[Vec<f32>]) -> (Vec<f32>, Vec<bool>) {
        let mut flat = Vec::with_capacity(queries.len().saturating_mul(self.dim));
        let mut valid = Vec::with_capacity(queries.len());
        for q in queries {
            match normalized(q, self.dim) {
                Ok(v) => {
                    flat.extend_from_slice(&v);
                    valid.push(true);
                }
                Err(_) => valid.push(false),
            }
        }
        (flat, valid)
    }

    fn spread(res: Vec<Vec<Hit>>, valid: &[bool], n: usize) -> Vec<Vec<Hit>> {
        let mut it = res.into_iter();
        let mut out = Vec::with_capacity(n);
        for &ok in valid {
            out.push(if ok {
                it.next().unwrap_or_default()
            } else {
                Vec::new()
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::Mulberry32;

    fn gauss(r: &mut Mulberry32) -> f32 {
        let u = r.next_f64().max(1e-12);
        let v = r.next_f64();
        ((-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()) as f32
    }

    fn random_matrix(n: usize, dim: usize, seed: u32) -> Vec<f32> {
        let mut r = Mulberry32::new(seed);
        let mut m: Vec<f32> = (0..n * dim).map(|_| gauss(&mut r)).collect();
        for row in m.chunks_exact_mut(dim) {
            normalize(row).unwrap();
        }
        m
    }

    fn brute(query: &[f32], matrix: &[f32], dim: usize, k: usize) -> Vec<Hit> {
        let mut all: Vec<Hit> = matrix
            .chunks_exact(dim)
            .enumerate()
            .map(|(i, r)| Hit {
                index: i,
                score: dot(query, r),
            })
            .collect();
        all.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.index.cmp(&b.index)));
        all.truncate(k);
        all
    }

    #[test]
    fn dot_and_cosine_basics() {
        let a: Vec<f32> = (0..384).map(|i| i as f32 * 0.01).collect();
        let naive: f32 = a.iter().map(|x| x * x).sum();
        assert!((dot(&a, &a) - naive).abs() / naive < 1e-5);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&a, &a[..10]), 0.0);
        assert_eq!(cosine(&[0.0; 4], &[1.0; 4]), 0.0);
        assert_eq!(cosine(&[f32::NAN; 4], &[1.0; 4]), 0.0);
        assert_eq!(dot(&[1.0], &[1.0, 2.0]), 0.0);
        let mut z = vec![0.0f32; 3];
        assert_eq!(normalize(&mut z), Err(VectorError::ZeroNorm));
        let mut n = vec![1.0, f32::INFINITY];
        assert_eq!(normalize(&mut n), Err(VectorError::NonFinite));
    }

    #[test]
    fn top_k_matches_brute_force() {
        let dim = 48;
        let m = random_matrix(500, dim, 7);
        let qs = random_matrix(40, dim, 8);
        let batch = batch_top_k(&qs, &m, dim, 10);
        assert_eq!(batch.len(), 40);
        for (qi, q) in qs.chunks_exact(dim).enumerate() {
            let want = brute(q, &m, dim, 10);
            let got = top_k(q, &m, dim, 10);
            assert_eq!(
                got.iter().map(|h| h.index).collect::<Vec<_>>(),
                want.iter().map(|h| h.index).collect::<Vec<_>>()
            );
            assert_eq!(batch[qi], got);
        }
    }

    #[test]
    fn ties_break_by_index() {
        let m = vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        let hits = top_k(&[1.0, 0.0], &m, 2, 2);
        assert_eq!(hits.iter().map(|h| h.index).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn int8_rerank_recall_at_least_99_percent() {
        let dim = 384;
        let m = random_matrix(4000, dim, 11);
        let q = QuantizedMatrix::from_f32(&m, dim).unwrap();
        assert_eq!(q.len(), 4000);
        let qs = random_matrix(100, dim, 12);
        let (mut hit, mut total) = (0usize, 0usize);
        for query in qs.chunks_exact(dim) {
            let want: Vec<usize> = brute(query, &m, dim, 10).iter().map(|h| h.index).collect();
            let got: Vec<usize> = quantized_top_k(query, &m, &q, 10, 4)
                .iter()
                .map(|h| h.index)
                .collect();
            total += want.len();
            hit += want.iter().filter(|i| got.contains(i)).count();
        }
        let recall = hit as f64 / total as f64;
        assert!(recall >= 0.99, "recall {recall}");
    }

    #[test]
    fn index_build_search_filter_and_similar() {
        let items = vec![
            ("a".to_string(), vec![1.0, 0.0, 0.0]),
            ("b".to_string(), vec![0.9, 0.1, 0.0]),
            ("c".to_string(), vec![0.0, 1.0, 0.0]),
            ("bad".to_string(), vec![f32::NAN, 0.0, 0.0]),
            ("short".to_string(), vec![1.0]),
            ("zero".to_string(), vec![0.0, 0.0, 0.0]),
        ];
        let (mut idx, rejected) = VectorIndex::build(3, items).unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(rejected.len(), 3);
        let hits = idx.search(&[1.0, 0.0, 0.0], 2);
        assert_eq!(
            idx.resolve(&hits).iter().map(|x| x.0).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let f = idx.search_filtered(&[1.0, 0.0, 0.0], 2, |_, id| id != "a");
        assert_eq!(idx.id(f[0].index), Some("b"));
        let sim = idx.similar_batch(&["a", "nope", "c"], 1);
        assert_eq!(sim.len(), 3);
        assert_eq!(idx.id(sim[0][0].index), Some("b"));
        assert!(sim[1].is_empty());
        idx.quantize().unwrap();
        let qh = idx.search_quantized(&[1.0, 0.0, 0.0], 2, 4);
        assert_eq!(
            qh.iter().map(|h| h.index).collect::<Vec<_>>(),
            hits.iter().map(|h| h.index).collect::<Vec<_>>()
        );
        // upsert replaces and drops the stale quantised copy
        idx.insert("a", &[0.0, 0.0, 1.0]).unwrap();
        assert!(!idx.is_quantized());
        assert_eq!(idx.len(), 3);
        assert!(idx.memory_bytes() > 0);
        assert!(estimate_memory_bytes(30_000, 384, 36, true) > 30_000 * 384 * 5);
    }

    #[test]
    fn hostile_inputs_never_panic() {
        assert!(top_k(&[], &[], 0, 5).is_empty());
        assert!(top_k(&[1.0], &[1.0, 2.0, 3.0], 2, 5).is_empty());
        assert!(top_k(&[f32::NAN, 0.0], &[1.0, 0.0], 2, 5).is_empty());
        assert_eq!(top_k(&[1.0, 0.0], &[1.0, 0.0], 2, 0).len(), 0);
        assert_eq!(top_k(&[1.0, 0.0], &[1.0, 0.0, 0.0, 1.0], 2, 99).len(), 2); // k > n
        assert_eq!(
            top_k(&[1.0, 0.0], &[f32::NAN, 0.0, 0.0, 1.0], 2, 2).len(),
            1
        ); // NaN row skipped
        assert!(batch_top_k(&[1.0, 2.0, 3.0], &[1.0, 0.0], 2, 3).is_empty());
        let r = batch_top_k(&[f32::NAN, 0.0, 1.0, 0.0], &[1.0, 0.0], 2, 3);
        assert!(r[0].is_empty() && r[1].len() == 1);
        assert!(VectorIndex::new(0).is_err());
        assert!(VectorIndex::new(MAX_DIM + 1).is_err());
        let idx = VectorIndex::new(4).unwrap();
        assert!(idx.search(&[1.0, 0.0, 0.0, 0.0], 3).is_empty());
        assert!(idx.search(&[1.0], 3).is_empty());
        assert!(
            idx.search_batch(&[vec![1.0], vec![]], 3)
                .iter()
                .all(Vec::is_empty)
        );
        assert!(idx.search_quantized(&[0.0; 4], 3, 0).is_empty());
        assert!(QuantizedMatrix::from_f32(&[1.0, 2.0, 3.0], 2).is_err());
        assert_eq!(quantize_vector(&[0.0, f32::NAN]), (vec![0, 0], 0.0));
        assert_eq!(dot_i8(&[1], &[1, 2]), 0);
        let q = QuantizedMatrix::from_f32(&[], 3).unwrap();
        assert!(quantized_top_k(&[1.0, 0.0, 0.0], &[], &q, 3, 2).is_empty());
    }

    /// 3,000 queries x 30,000 x 384 top-10. Run with
    /// `cargo test -p sf-engine --release -- --ignored perf_ --nocapture`.
    #[test]
    #[ignore = "performance measurement"]
    fn perf_batch_top10_3k_queries_over_30k() {
        let dim = 384;
        let m = random_matrix(30_000, dim, 21);
        let qs = random_matrix(3_000, dim, 22);
        let t = std::time::Instant::now();
        let res = crate::pool::install(|| batch_top_k(&qs, &m, dim, 10));
        let exact = t.elapsed();
        assert_eq!(res.len(), 3_000);
        let quant = QuantizedMatrix::from_f32(&m, dim).unwrap();
        let t = std::time::Instant::now();
        let qres: Vec<Vec<Hit>> = crate::pool::install(|| {
            qs.par_chunks(dim)
                .map(|q| quantized_top_k(q, &m, &quant, 10, 4))
                .collect()
        });
        let q_elapsed = t.elapsed();
        assert_eq!(qres.len(), 3_000);
        eprintln!(
            "perf: exact batch {exact:?}, int8+rerank {q_elapsed:?}, pool threads {}",
            crate::pool::threads()
        );
    }
}
