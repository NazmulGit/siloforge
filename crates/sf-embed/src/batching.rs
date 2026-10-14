//! Length-sorted sub-batching and mean pooling, shared by the local backends.
//!
//! Sorting by token length before batching removes most padding work (3-4x faster on CPU, see
//! docs/research/06). Outputs are written back to the caller's order.
//!
//! [`HybridJob`] is the work-stealing scheduler of the hybrid (GPU + CPU) onnx rung. It is
//! generic over the model call so it is unit-tested without ONNX Runtime.
#![cfg_attr(not(feature = "onnx"), allow(dead_code))]

use std::collections::VecDeque;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use sf_core::{EMBED_DIM, Error, Result};

/// Input indices sorted by token length (stable: ties keep input order, deterministic).
pub(crate) fn sorted_order(ids: &[Vec<i64>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ids.len()).collect();
    order.sort_by_key(|&i| ids.get(i).map_or(0, Vec::len));
    order
}

/// Pad the inputs `chunk` (indices into `ids`) to one `[b, l]` batch, run `hidden` and pool.
/// Returns one vector per entry of `chunk`, in `chunk` order.
pub(crate) fn run_chunk<F>(
    ids: &[Vec<i64>],
    chunk: &[usize],
    pad_to: usize,
    hidden: &mut F,
) -> Result<Vec<Vec<f32>>>
where
    F: FnMut(&[i64], &[i64], usize, usize) -> Result<Vec<f32>>,
{
    if chunk.is_empty() {
        return Ok(Vec::new());
    }
    let pad_to = pad_to.max(1);
    let longest = chunk
        .iter()
        .map(|&i| ids.get(i).map_or(0, Vec::len))
        .max()
        .unwrap_or(1)
        .max(1);
    let l = longest.div_ceil(pad_to).saturating_mul(pad_to);
    let b = chunk.len();
    let cells = b
        .checked_mul(l)
        .ok_or_else(|| Error::internal("batch size overflow"))?;
    let mut flat_ids = vec![0i64; cells];
    let mut mask = vec![0i64; cells];
    for (r, &i) in chunk.iter().enumerate() {
        let row = ids.get(i).map_or(&[][..], Vec::as_slice);
        for (t, &id) in row.iter().enumerate().take(l) {
            if let (Some(slot), Some(m)) = (flat_ids.get_mut(r * l + t), mask.get_mut(r * l + t)) {
                *slot = id;
                *m = 1;
            }
        }
    }
    let h = hidden(&flat_ids, &mask, b, l)?;
    pool(&h, &mask, b, l)
}

/// Run `hidden` over length-sorted sub-batches of at most `batch` sequences.
///
/// `ids[i]` are the token ids of input `i` (attention mask is all ones over them). `pad_to` rounds the
/// padded length up to a multiple (limits distinct shapes on GPU EPs; use 1 on CPU). `hidden` gets
/// `(ids, mask, b, l)` row-major `[b * l]` and returns `last_hidden_state` flattened `[b * l * 384]`.
pub(crate) fn run_sorted<F>(
    ids: &[Vec<i64>],
    batch: usize,
    pad_to: usize,
    mut hidden: F,
) -> Result<Vec<Vec<f32>>>
where
    F: FnMut(&[i64], &[i64], usize, usize) -> Result<Vec<f32>>,
{
    let order = sorted_order(ids);
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); ids.len()];
    for chunk in order.chunks(batch.max(1)) {
        let pooled = run_chunk(ids, chunk, pad_to, &mut hidden)?;
        for (r, v) in pooled.into_iter().enumerate() {
            if let Some(&i) = chunk.get(r)
                && let Some(slot) = out.get_mut(i)
            {
                *slot = v;
            }
        }
    }
    Ok(out)
}

/// Which worker of a hybrid job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Gpu,
    Cpu,
}

/// Shape of a hybrid job (see [`HybridJob`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct HybridPlan {
    /// Sequences per queue chunk (the CPU worker runs one chunk at a time).
    pub chunk: usize,
    /// Most sequences the GPU worker takes in one run.
    pub gpu_batch: usize,
    pub gpu_pad_to: usize,
    pub cpu_pad_to: usize,
    /// Share of the items the GPU should take (0.1..0.9). While the GPU worker is alive the CPU
    /// may not take more than `1 - gpu_share` of the items; the GPU may take everything.
    pub gpu_share: f32,
}

/// How a finished hybrid job was split between the workers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HybridSplit {
    pub gpu_items: usize,
    pub cpu_items: usize,
}

#[derive(Debug, Default)]
struct SideState {
    /// Still taking work (false after an error, or once its worker returned).
    alive: bool,
    /// The grab it is running right now (handed back on failure or panic).
    inflight: Vec<Vec<usize>>,
    items: usize,
    failed: bool,
}

struct QueueState {
    /// Chunks of length-sorted indices: front = shortest (CPU end), back = longest (GPU end).
    pending: VecDeque<Vec<usize>>,
    out: Vec<Option<Vec<f32>>>,
    gpu: SideState,
    cpu: SideState,
    errors: Vec<String>,
}

impl QueueState {
    fn side(&mut self, s: Side) -> &mut SideState {
        match s {
            Side::Gpu => &mut self.gpu,
            Side::Cpu => &mut self.cpu,
        }
    }

    fn give_back(&mut self, from: Side, chunks: Vec<Vec<usize>>) {
        for c in chunks {
            match from {
                Side::Gpu => self.pending.push_front(c),
                Side::Cpu => self.pending.push_back(c),
            }
        }
    }

    fn other(&self, s: Side) -> &SideState {
        match s {
            Side::Gpu => &self.cpu,
            Side::Cpu => &self.gpu,
        }
    }
}

/// One work-stealing job shared by a GPU worker and a CPU worker.
///
/// The inputs are sorted by length and cut into chunks of `plan.chunk`. The GPU worker takes
/// several chunks at once from the long end, the CPU worker single chunks from the short end. A
/// worker that fails hands its chunks back and stops; the other one finishes the job. Results go
/// back to input order.
pub(crate) struct HybridJob {
    ids: Vec<Vec<i64>>,
    plan: HybridPlan,
    state: Mutex<QueueState>,
    cv: Condvar,
}

/// Marks a side as gone on drop (also when its worker panics), so the other side never waits
/// for it.
struct ExitGuard<'a> {
    job: &'a HybridJob,
    side: Side,
}

impl Drop for ExitGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.job.state.lock();
        let side = self.side;
        let s = st.side(side);
        s.alive = false;
        let lost = std::mem::take(&mut s.inflight);
        if !lost.is_empty() {
            // the worker died inside a model call (panic): give its chunks to the other side
            s.failed = true;
            st.errors.push(format!("{side:?}: worker crashed"));
            st.give_back(side, lost);
        }
        drop(st);
        self.job.cv.notify_all();
    }
}

impl HybridJob {
    /// `gpu` / `cpu`: whether that worker will run at all.
    pub(crate) fn new(ids: Vec<Vec<i64>>, plan: HybridPlan, gpu: bool, cpu: bool) -> Self {
        let order = sorted_order(&ids);
        let pending = order
            .chunks(plan.chunk.max(1))
            .map(<[usize]>::to_vec)
            .collect();
        let n = ids.len();
        Self {
            ids,
            plan,
            state: Mutex::new(QueueState {
                pending,
                out: vec![None; n],
                gpu: SideState {
                    alive: gpu,
                    ..SideState::default()
                },
                cpu: SideState {
                    alive: cpu,
                    ..SideState::default()
                },
                errors: Vec::new(),
            }),
            cv: Condvar::new(),
        }
    }

    fn share(&self) -> f64 {
        clamp_share(self.plan.gpu_share)
    }

    /// Next grab for `side`, or None when it has to wait (or nothing is left).
    fn take(&self, st: &mut QueueState, side: Side) -> Option<Vec<Vec<usize>>> {
        let first_len = match side {
            Side::Gpu => st.pending.back(),
            Side::Cpu => st.pending.front(),
        }
        .map(Vec::len)?;
        match side {
            Side::Cpu => {
                if st.gpu.alive {
                    let cap = ((1.0 - self.share()) * self.ids.len() as f64).ceil() as usize;
                    if st.cpu.items.saturating_add(first_len) > cap {
                        return None;
                    }
                }
                st.pending.pop_front().map(|c| vec![c])
            }
            Side::Gpu => {
                let gpu_batch = self.plan.gpu_batch.max(1);
                let limit = if st.cpu.alive {
                    // leave the CPU something to do, but never less than one chunk per grab
                    let remaining: usize = st.pending.iter().map(Vec::len).sum();
                    let fair = (self.share() * remaining as f64).ceil() as usize;
                    gpu_batch.min(fair.max(1))
                } else {
                    gpu_batch
                };
                let mut grab = Vec::new();
                let mut total = 0usize;
                while let Some(len) = st.pending.back().map(Vec::len) {
                    if !grab.is_empty() && total.saturating_add(len) > limit {
                        break;
                    }
                    let Some(c) = st.pending.pop_back() else {
                        break;
                    };
                    total = total.saturating_add(len);
                    grab.push(c);
                }
                (!grab.is_empty()).then_some(grab)
            }
        }
    }

    /// Worker loop for one side; returns when the queue is drained or this side failed.
    pub(crate) fn work<F>(&self, side: Side, mut hidden: F)
    where
        F: FnMut(&[i64], &[i64], usize, usize) -> Result<Vec<f32>>,
    {
        let _exit = ExitGuard { job: self, side };
        let pad_to = match side {
            Side::Gpu => self.plan.gpu_pad_to,
            Side::Cpu => self.plan.cpu_pad_to,
        };
        loop {
            let grab = {
                let mut st = self.state.lock();
                loop {
                    if !st.side(side).alive {
                        return;
                    }
                    if let Some(g) = self.take(&mut st, side) {
                        let idx: Vec<usize> = g.iter().flatten().copied().collect();
                        st.side(side).inflight = g;
                        break idx;
                    }
                    // Done when nothing is left and the other side cannot hand work back.
                    if st.pending.is_empty() && st.other(side).inflight.is_empty() {
                        return;
                    }
                    // Bounded wait: re-check even if a wake-up is missed.
                    self.cv.wait_for(&mut st, Duration::from_millis(50));
                }
            };
            let idx = grab;
            let r = run_chunk(&self.ids, &idx, pad_to, &mut hidden);
            let mut st = self.state.lock();
            let grab = std::mem::take(&mut st.side(side).inflight);
            match r {
                Ok(vs) if vs.len() == idx.len() => {
                    for (&i, v) in idx.iter().zip(vs) {
                        if let Some(slot) = st.out.get_mut(i) {
                            *slot = Some(v);
                        }
                    }
                    let s = st.side(side);
                    s.items = s.items.saturating_add(idx.len());
                    drop(st);
                    self.cv.notify_all();
                }
                other => {
                    let msg = match other {
                        Err(e) => e.to_string(),
                        Ok(vs) => format!("{} vectors for {} inputs", vs.len(), idx.len()),
                    };
                    tracing::warn!(side = ?side, error = %msg, "hybrid embed worker failed; the other worker takes over");
                    st.errors.push(format!("{side:?}: {msg}"));
                    let s = st.side(side);
                    s.alive = false;
                    s.failed = true;
                    // hand the chunks back to the end the other side reads from
                    st.give_back(side, grab);
                    drop(st);
                    self.cv.notify_all();
                    return;
                }
            }
        }
    }

    /// Did `side` fail during this job?
    pub(crate) fn failed(&self, side: Side) -> bool {
        self.state.lock().side(side).failed
    }

    /// Collect the vectors (input order) once both workers have returned.
    pub(crate) fn finish(&self) -> Result<(Vec<Vec<f32>>, HybridSplit)> {
        let mut st = self.state.lock();
        let split = HybridSplit {
            gpu_items: st.gpu.items,
            cpu_items: st.cpu.items,
        };
        let out = std::mem::take(&mut st.out);
        let missing = out.iter().filter(|v| v.is_none()).count();
        if missing > 0 {
            let why = if st.errors.is_empty() {
                "no worker available".to_string()
            } else {
                st.errors.join("; ")
            };
            return Err(Error::internal(format!(
                "hybrid embed: {missing} of {} inputs not embedded: {why}",
                out.len()
            )));
        }
        Ok((out.into_iter().flatten().collect(), split))
    }
}

/// `hybrid_gpu_share` clamped to 0.1..0.9 (NaN/inf -> the 0.7 default).
pub(crate) fn clamp_share(share: f32) -> f64 {
    let s = f64::from(share);
    if s.is_finite() {
        s.clamp(0.1, 0.9)
    } else {
        0.7
    }
}

/// Mean pooling over the attention mask + L2 normalisation.
pub(crate) fn pool(h: &[f32], mask: &[i64], b: usize, l: usize) -> Result<Vec<Vec<f32>>> {
    let want = b
        .checked_mul(l)
        .and_then(|x| x.checked_mul(EMBED_DIM))
        .ok_or_else(|| Error::internal("hidden state size overflow"))?;
    if h.len() != want || mask.len() != b * l {
        return Err(Error::internal(format!(
            "model returned {} hidden values, expected {want} ({b}x{l}x{EMBED_DIM})",
            h.len()
        )));
    }
    let mut out = Vec::with_capacity(b);
    for i in 0..b {
        let mut v = vec![0f32; EMBED_DIM];
        let mut n = 0f32;
        for t in 0..l {
            if mask.get(i * l + t).copied().unwrap_or(0) == 0 {
                continue;
            }
            n += 1.0;
            let start = (i * l + t) * EMBED_DIM;
            if let Some(row) = h.get(start..start + EMBED_DIM) {
                for (a, x) in v.iter_mut().zip(row) {
                    *a += x;
                }
            }
        }
        let n = n.max(1e-9);
        v.iter_mut().for_each(|x| *x /= n);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter_mut().for_each(|x| *x /= norm);
        out.push(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake model: every hidden row of a real token = one-hot-ish vector derived from the sequence's
    /// first token id, so the pooled output identifies which input it came from.
    fn fake_hidden(ids: &[i64], mask: &[i64], b: usize, l: usize) -> Result<Vec<f32>> {
        let mut h = vec![0f32; b * l * EMBED_DIM];
        for r in 0..b {
            let tag = ids[r * l] as usize % EMBED_DIM;
            for t in 0..l {
                // padding rows get garbage: pooling must ignore them
                let val = if mask[r * l + t] == 1 { 1.0 } else { 1000.0 };
                let k = if mask[r * l + t] == 1 { tag } else { 0 };
                h[(r * l + t) * EMBED_DIM + k] = val;
            }
        }
        Ok(h)
    }

    #[test]
    fn sorted_batching_preserves_order() {
        // lengths deliberately unsorted; first token = tag
        let lens = [7usize, 1, 30, 3, 3, 12, 2, 9, 1, 25];
        let ids: Vec<Vec<i64>> = lens
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let mut v = vec![(i + 10) as i64];
                v.extend(std::iter::repeat_n(5i64, n.saturating_sub(1)));
                v
            })
            .collect();
        let mut calls = Vec::new();
        let out = run_sorted(&ids, 3, 1, |a, m, b, l| {
            calls.push((b, l));
            fake_hidden(a, m, b, l)
        })
        .unwrap();
        assert_eq!(out.len(), ids.len());
        for (i, v) in out.iter().enumerate() {
            let hot = v.iter().position(|x| (*x - 1.0).abs() < 1e-5);
            assert_eq!(hot, Some(i + 10), "row {i}");
        }
        // sorted: padded lengths are non-decreasing across sub-batches
        let ls: Vec<usize> = calls.iter().map(|c| c.1).collect();
        assert!(ls.windows(2).all(|w| w[0] <= w[1]), "{ls:?}");
        assert_eq!(calls.len(), 4);
    }

    #[test]
    fn pad_to_rounds_up() {
        let ids = vec![vec![1i64; 5], vec![2i64; 33]];
        let mut ls = Vec::new();
        run_sorted(&ids, 1, 32, |a, m, b, l| {
            ls.push(l);
            fake_hidden(a, m, b, l)
        })
        .unwrap();
        assert_eq!(ls, vec![32, 64]);
    }

    #[test]
    fn wrong_hidden_size_is_an_error_not_a_panic() {
        let ids = vec![vec![1i64; 4]];
        let r = run_sorted(&ids, 8, 1, |_, _, _, _| Ok(vec![0.0; 7]));
        assert!(r.is_err());
        let empty: Vec<Vec<i64>> = Vec::new();
        assert!(run_sorted(&empty, 8, 1, fake_hidden).unwrap().is_empty());
        // empty token row still yields a vector
        let r = run_sorted(&[Vec::new()], 8, 1, |_, _, b, l| {
            Ok(vec![0.0; b * l * EMBED_DIM])
        });
        assert_eq!(r.unwrap()[0].len(), EMBED_DIM);
    }

    fn tagged_ids(n: usize) -> Vec<Vec<i64>> {
        // first token = tag (input index + 10), lengths vary and are unsorted
        (0..n)
            .map(|i| {
                let len = 1 + (i * 37) % 50;
                let mut v = vec![(i + 10) as i64];
                v.extend(std::iter::repeat_n(5i64, len - 1));
                v
            })
            .collect()
    }

    fn plan(share: f32) -> HybridPlan {
        HybridPlan {
            chunk: 8,
            gpu_batch: 64,
            gpu_pad_to: 32,
            cpu_pad_to: 1,
            gpu_share: share,
        }
    }

    fn check_order(out: &[Vec<f32>]) {
        for (i, v) in out.iter().enumerate() {
            let hot = v.iter().position(|x| (*x - 1.0).abs() < 1e-5);
            assert_eq!(hot, Some(i + 10), "row {i}");
        }
    }

    type Hidden = Box<dyn FnMut(&[i64], &[i64], usize, usize) -> Result<Vec<f32>> + Send>;

    fn slow(ms: u64) -> Hidden {
        Box::new(move |a, m, b, l| {
            std::thread::sleep(Duration::from_millis(ms));
            fake_hidden(a, m, b, l)
        })
    }

    /// Run both workers on their own threads (like the two spawn_blocking tasks).
    fn run_job(job: &HybridJob, gpu: Option<Hidden>, cpu: Option<Hidden>) {
        std::thread::scope(|s| {
            let g = gpu.map(|h| s.spawn(move || job.work(Side::Gpu, h)));
            let c = cpu.map(|h| s.spawn(move || job.work(Side::Cpu, h)));
            if let Some(g) = g {
                let _ = g.join();
            }
            if let Some(c) = c {
                let _ = c.join();
            }
        });
    }

    #[test]
    fn hybrid_split_respects_share_and_keeps_order() {
        for share in [0.5f32, 0.7, 0.9] {
            let n = 160;
            let job = HybridJob::new(tagged_ids(n), plan(share), true, true);
            // CPU is much faster than the GPU here: only the quota stops it
            run_job(&job, Some(slow(30)), Some(slow(0)));
            let (out, split) = job.finish().unwrap();
            check_order(&out);
            assert_eq!(split.gpu_items + split.cpu_items, n);
            let cap = ((1.0 - f64::from(share)) * n as f64).ceil() as usize;
            assert!(split.cpu_items <= cap, "share {share}: {split:?}");
            assert!(split.cpu_items > 0, "share {share}: {split:?}");
        }
    }

    #[test]
    fn hybrid_gpu_steals_from_a_slow_cpu() {
        let n = 160;
        let job = HybridJob::new(tagged_ids(n), plan(0.5), true, true);
        run_job(&job, Some(slow(1)), Some(slow(80)));
        let (out, split) = job.finish().unwrap();
        check_order(&out);
        assert!(split.gpu_items > n / 2, "{split:?}");
    }

    #[test]
    fn hybrid_gpu_failure_cpu_finishes() {
        let n = 100;
        let job = HybridJob::new(tagged_ids(n), plan(0.7), true, true);
        let gpu: Hidden = Box::new(|_, _, _, _| Err(Error::internal("device removed")));
        run_job(&job, Some(gpu), Some(slow(2)));
        assert!(job.failed(Side::Gpu));
        assert!(!job.failed(Side::Cpu));
        let (out, split) = job.finish().unwrap();
        check_order(&out);
        assert_eq!(
            split,
            HybridSplit {
                gpu_items: 0,
                cpu_items: n
            }
        );
    }

    #[test]
    fn hybrid_gpu_fails_mid_job_after_some_work() {
        let n = 200;
        let job = HybridJob::new(tagged_ids(n), plan(0.7), true, true);
        let mut calls = 0;
        let gpu: Hidden = Box::new(move |a, m, b, l| {
            calls += 1;
            std::thread::sleep(Duration::from_millis(5));
            if calls > 1 {
                return Err(Error::internal("lost device"));
            }
            fake_hidden(a, m, b, l)
        });
        run_job(&job, Some(gpu), Some(slow(5)));
        let (out, split) = job.finish().unwrap();
        check_order(&out);
        assert!(split.gpu_items > 0 && split.cpu_items > 0, "{split:?}");
        assert_eq!(split.gpu_items + split.cpu_items, n);
    }

    #[test]
    fn hybrid_cpu_failure_gpu_finishes() {
        let n = 90;
        let job = HybridJob::new(tagged_ids(n), plan(0.7), true, true);
        let cpu: Hidden = Box::new(|_, _, _, _| Err(Error::internal("oom")));
        run_job(&job, Some(slow(10)), Some(cpu));
        let (out, split) = job.finish().unwrap();
        check_order(&out);
        assert_eq!(split.cpu_items, 0);
        assert!(job.failed(Side::Cpu));
    }

    #[test]
    fn hybrid_single_side_and_both_failing() {
        let n = 50;
        // GPU absent: the CPU has no quota
        let job = HybridJob::new(tagged_ids(n), plan(0.9), false, true);
        run_job(&job, None, Some(slow(0)));
        let (out, split) = job.finish().unwrap();
        check_order(&out);
        assert_eq!(
            split,
            HybridSplit {
                gpu_items: 0,
                cpu_items: n
            }
        );

        // CPU absent: GPU grabs at most gpu_batch sequences per run
        let job = HybridJob::new(tagged_ids(n), plan(0.5), true, false);
        let sizes = std::sync::Arc::new(Mutex::new(Vec::new()));
        let s2 = sizes.clone();
        let gpu: Hidden = Box::new(move |a, m, b, l| {
            s2.lock().push((b, l));
            fake_hidden(a, m, b, l)
        });
        run_job(&job, Some(gpu), None);
        let (out, _) = job.finish().unwrap();
        check_order(&out);
        for (b, l) in sizes.lock().iter() {
            assert!(*b <= 64 && l % 32 == 0, "{b}x{l}");
        }

        // both fail: a clean error, no hang
        let job = HybridJob::new(tagged_ids(n), plan(0.7), true, true);
        let bad = || -> Hidden { Box::new(|_, _, _, _| Err(Error::internal("nope"))) };
        run_job(&job, Some(bad()), Some(bad()));
        let e = job.finish().unwrap_err().to_string();
        assert!(e.contains("not embedded") && e.contains("nope"), "{e}");

        // nothing runs at all
        let job = HybridJob::new(tagged_ids(3), plan(0.7), false, false);
        assert!(job.finish().is_err());
        // empty input
        let job = HybridJob::new(Vec::new(), plan(0.7), true, true);
        run_job(&job, Some(slow(0)), Some(slow(0)));
        assert!(job.finish().unwrap().0.is_empty());
    }

    #[test]
    fn hybrid_worker_panic_does_not_hang_the_other() {
        let n = 64;
        let job = HybridJob::new(tagged_ids(n), plan(0.7), true, true);
        let gpu: Hidden = Box::new(|_, _, _, _| -> Result<Vec<f32>> {
            std::panic::panic_any("simulated gpu worker crash")
        });
        run_job(&job, Some(gpu), Some(slow(1)));
        // the crashed grab goes back to the queue and the CPU finishes everything
        assert!(job.failed(Side::Gpu));
        let (out, split) = job.finish().unwrap();
        check_order(&out);
        assert_eq!(split.cpu_items, n);
    }

    #[test]
    fn share_is_clamped() {
        assert_eq!(clamp_share(0.0), 0.1);
        assert_eq!(clamp_share(2.0), 0.9);
        assert_eq!(clamp_share(f32::NAN), 0.7);
        assert!((clamp_share(0.7) - 0.7).abs() < 1e-6);
    }
}
