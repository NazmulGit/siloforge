//! Length-sorted sub-batching and mean pooling, shared by the local backends.
//!
//! Sorting by token length before batching removes most padding work (3-4x faster on CPU, see
//! docs/research/06). Outputs are written back to the caller's order.
#![cfg_attr(not(any(feature = "onnx", feature = "candle")), allow(dead_code))]

use sf_core::{EMBED_DIM, Error, Result};

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
    let mut order: Vec<usize> = (0..ids.len()).collect();
    // stable sort, ties keep input order (deterministic)
    order.sort_by_key(|&i| ids.get(i).map_or(0, Vec::len));
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); ids.len()];
    let pad_to = pad_to.max(1);
    for chunk in order.chunks(batch.max(1)) {
        let longest = chunk
            .iter()
            .map(|&i| ids.get(i).map_or(0, Vec::len))
            .max()
            .unwrap_or(1)
            .max(1);
        let l = longest.div_ceil(pad_to).saturating_mul(pad_to);
        let b = chunk.len();
        let mut flat_ids = vec![0i64; b * l];
        let mut mask = vec![0i64; b * l];
        for (r, &i) in chunk.iter().enumerate() {
            let row = ids.get(i).map_or(&[][..], Vec::as_slice);
            for (t, &id) in row.iter().enumerate().take(l) {
                if let (Some(slot), Some(m)) =
                    (flat_ids.get_mut(r * l + t), mask.get_mut(r * l + t))
                {
                    *slot = id;
                    *m = 1;
                }
            }
        }
        let h = hidden(&flat_ids, &mask, b, l)?;
        let pooled = pool(&h, &mask, b, l)?;
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
}
