//! `GET /reports/metrics/:siteId?trend=N`: latest snapshot, deltas vs the previous one, trend (ASC).

use axum::Json;
use axum::extract::{Path, State};
use serde_json::{Value, json};

use super::common::{Params, guard_user_site, int_param, num, query_json};
use crate::auth::User;
use crate::{ApiResult, AppState};

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Up,
    Down,
    Neutral,
}

/// Metric keys in v1 order with the direction that counts as "improved".
const METRICS: &[(&str, Dir)] = &[
    ("posts", Dir::Neutral),
    ("clusters", Dir::Neutral),
    ("edges_suggested", Dir::Neutral),
    ("orphans", Dir::Down),
    ("contextual_orphans", Dir::Down),
    ("nav_only_orphans", Dir::Down),
    ("dead_ends", Dir::Down),
    ("avg_click_depth", Dir::Down),
    ("max_click_depth", Dir::Down),
    ("components", Dir::Down),
    ("broken_links", Dir::Down),
    ("nofollow_links", Dir::Down),
    ("redirect_links", Dir::Down),
    ("redirect_chains_over_2", Dir::Down),
    ("link_overload_pages", Dir::Down),
    ("posts_under_min_links", Dir::Down),
    ("internal_links", Dir::Up),
    ("avg_links_per_post", Dir::Up),
    ("edges_applied", Dir::Up),
    ("health_score", Dir::Up),
];

fn metric(v: &Value, key: &str) -> Option<f64> {
    match v.get("metrics")?.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|f| f.is_finite())
}

/// Delta rows for the latest snapshot vs its predecessor (keys with neither value are skipped).
fn deltas(latest: Option<&Value>, prev: Option<&Value>) -> Vec<Value> {
    let Some(latest) = latest else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, dir) in METRICS {
        let cur = metric(latest, key);
        let old = prev.and_then(|p| metric(p, key));
        if cur.is_none() && old.is_none() {
            continue;
        }
        let delta = match (cur, old) {
            (Some(a), Some(b)) => Some(((a - b) * 1e9).round() / 1e9),
            _ => None,
        };
        let pct = match (delta, old) {
            (Some(d), Some(b)) if b != 0.0 => Some((100.0 * d / b * 100.0).round() / 100.0),
            _ => None,
        };
        let improved = match delta {
            None => Value::Null,
            Some(d) if d == 0.0 || *dir == Dir::Neutral => Value::Null,
            Some(d) => Value::Bool(if *dir == Dir::Up { d > 0.0 } else { d < 0.0 }),
        };
        out.push(json!({
            "key": key,
            "current": cur.map(num),
            "previous": old.map(num),
            "delta": delta.map(num),
            "pct": pct.map(num),
            "improved": improved,
        }));
    }
    out
}

pub async fn report(
    State(st): State<AppState>,
    u: User,
    Path(site_id): Path<String>,
    q: Params,
) -> ApiResult<Json<Value>> {
    guard_user_site(&st, &u, &site_id).await?;
    let limit = int_param(&q, "trend", 30, 1, 500);
    let v = st
        .db
        .read(move |c| {
            let mut recent = query_json(
                c,
                "SELECT id, site_id, job_run_id, source, metrics, captured_at FROM site_metrics_history
                  WHERE site_id=?1 ORDER BY captured_at DESC, id DESC LIMIT ?2",
                &[&site_id, &limit.max(2)],
                &["metrics"],
                &[],
            )?;
            let latest = recent.first().cloned();
            let d = deltas(recent.first(), recent.get(1));
            recent.truncate(usize::try_from(limit).unwrap_or(30));
            recent.reverse();
            Ok(json!({ "latest": latest, "deltas": d, "trend": recent }))
        })
        .await?;
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_directions() {
        let a = json!({"metrics": {"orphans": 3, "health_score": 80, "posts": 10}});
        let b =
            json!({"metrics": {"orphans": 5, "health_score": 70, "posts": 10, "broken_links": 0}});
        let d = deltas(Some(&a), Some(&b));
        let get = |k: &str| {
            d.iter()
                .find(|r| r["key"] == k)
                .cloned()
                .unwrap_or(Value::Null)
        };
        assert_eq!(get("orphans")["improved"], json!(true));
        assert_eq!(get("orphans")["pct"], json!(-40));
        assert_eq!(get("health_score")["improved"], json!(true));
        assert_eq!(get("posts")["improved"], Value::Null);
        assert_eq!(get("broken_links")["current"], Value::Null);
        assert!(deltas(None, None).is_empty());
    }
}
