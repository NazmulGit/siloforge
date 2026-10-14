//! Persisting settings changes (v2.1) without losing the operator's formatting.
//!
//! * `siloforge.toml` is edited with `toml_edit`: comments, key order and unknown keys/tables
//!   are kept; only the changed keys are written.
//! * `secrets.env` is rewritten line by line: only the `KEY=` lines that change are replaced
//!   (or appended / removed); every other line, comment and blank line is kept verbatim.
//!
//! Both writes go through a temp file + rename so a crash never leaves half a file.

use std::path::Path;

use crate::{Error, Result};

/// One TOML change: `[table] key = value` (`table` may be dotted, e.g. `crawl.host_aliases`).
#[derive(Debug, Clone)]
pub struct TomlChange {
    pub table: String,
    pub key: String,
    pub value: serde_json::Value,
}

fn to_toml_value(v: &serde_json::Value) -> Result<toml_edit::Value> {
    use serde_json::Value as J;
    Ok(match v {
        J::Bool(b) => (*b).into(),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(f) = n.as_f64() {
                // keep floats short and stable (0.7 instead of 0.699999988079071)
                ((f * 1e6).round() / 1e6).into()
            } else {
                return Err(Error::bad("number out of range"));
            }
        }
        J::String(s) => s.as_str().into(),
        J::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(to_toml_value(x)?);
            }
            toml_edit::Value::Array(arr)
        }
        J::Object(o) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, x) in o {
                t.insert(k, to_toml_value(x)?);
            }
            toml_edit::Value::InlineTable(t)
        }
        J::Null => return Err(Error::bad("null is not a TOML value")),
    })
}

/// Apply `changes` to TOML text, keeping everything else. Returns the new text.
pub fn edit_toml_text(text: &str, changes: &[TomlChange]) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::bad(format!("siloforge.toml does not parse: {e}")))?;
    for c in changes {
        let mut item: &mut toml_edit::Item = doc.as_item_mut();
        for part in c.table.split('.').filter(|p| !p.is_empty()) {
            let tbl = item
                .as_table_like_mut()
                .ok_or_else(|| Error::bad(format!("[{}] is not a table", c.table)))?;
            if tbl.get(part).is_none() {
                tbl.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            item = tbl
                .get_mut(part)
                .ok_or_else(|| Error::internal("toml table vanished"))?;
        }
        let tbl = item
            .as_table_like_mut()
            .ok_or_else(|| Error::bad(format!("[{}] is not a table", c.table)))?;
        let new = to_toml_value(&c.value)?;
        match tbl.get_mut(&c.key) {
            // keep the existing key's decoration (trailing comment, spacing)
            Some(toml_edit::Item::Value(old)) => {
                let decor = old.decor().clone();
                *old = new;
                *old.decor_mut() = decor;
            }
            _ => {
                tbl.insert(&c.key, toml_edit::Item::Value(new));
            }
        }
    }
    Ok(doc.to_string())
}

fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Apply `changes` to the TOML file at `path` (created when missing).
pub fn edit_toml_file(path: &Path, changes: &[TomlChange]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let text = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let out = edit_toml_text(&text, changes)?;
    atomic_write(path, &out)
}

fn quote_env(v: &str) -> String {
    let plain = !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@+,".contains(c));
    if plain {
        v.to_string()
    } else {
        format!("\"{}\"", v.replace('"', ""))
    }
}

/// Set (`Some`) or remove (`None`) `KEY=` lines in env-file text. Only the touched lines change.
/// Values must be single-line; CR/LF are rejected. Double quotes are dropped from values.
pub fn edit_env_text(text: &str, changes: &[(String, Option<String>)]) -> Result<String> {
    for (k, v) in changes {
        let key_ok = !k.is_empty()
            && k.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !key_ok {
            return Err(Error::bad(format!("invalid env key {k}")));
        }
        if v.as_deref().is_some_and(|v| v.contains(['\n', '\r'])) {
            return Err(Error::bad(format!("{k}: value must be a single line")));
        }
    }
    let mut done = vec![false; changes.len()];
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        let body = t.strip_prefix("export ").unwrap_or(t);
        let key = if t.starts_with('#') {
            None
        } else {
            body.split_once('=').map(|(k, _)| k.trim())
        };
        let hit = key.and_then(|k| changes.iter().position(|(ck, _)| ck == k));
        match hit {
            Some(i) => {
                if done.get(i).copied().unwrap_or(true) {
                    continue; // a duplicate line of a changed key is dropped
                }
                if let Some(d) = done.get_mut(i) {
                    *d = true;
                }
                if let Some((k, Some(v))) = changes.get(i) {
                    out.push(format!("{k}={}", quote_env(v)));
                }
            }
            None => out.push(line.to_string()),
        }
    }
    for (i, (k, v)) in changes.iter().enumerate() {
        if !done.get(i).copied().unwrap_or(true)
            && let Some(v) = v
        {
            out.push(format!("{k}={}", quote_env(v)));
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    Ok(s)
}

/// Apply env changes to the file at `path` (created when missing).
pub fn edit_env_file(path: &Path, changes: &[(String, Option<String>)]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let text = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    atomic_write(path, &edit_env_text(&text, changes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ch(table: &str, key: &str, value: serde_json::Value) -> TomlChange {
        TomlChange {
            table: table.into(),
            key: key.into(),
            value,
        }
    }

    #[test]
    fn toml_edit_keeps_comments_and_unknown_keys() {
        let src = "# top comment\n[resources]\n# threads\ncpu_threads = 0 # inline\nmax_concurrent_jobs = 3\nmystery = \"keep\"\n\n[custom]\nx = 1\n";
        let out = edit_toml_text(
            src,
            &[
                ch("resources", "max_concurrent_jobs", json!(5)),
                ch("resources", "cpu_threads", json!(4)),
                ch("embed", "hybrid_gpu_share", json!(0.7f32)),
                ch("embed", "chain", json!(["onnx", "remote"])),
            ],
        )
        .unwrap();
        assert!(out.contains("# top comment") && out.contains("# threads"));
        assert!(out.contains("cpu_threads = 4 # inline"), "{out}");
        assert!(out.contains("max_concurrent_jobs = 5"));
        assert!(out.contains("mystery = \"keep\"") && out.contains("[custom]"));
        assert!(out.contains("hybrid_gpu_share = 0.7"), "{out}");
        let back: crate::Config = toml::from_str(&out).unwrap();
        assert_eq!(back.resources.max_concurrent_jobs, 5);
        assert_eq!(back.embed.chain, vec!["onnx", "remote"]);
        assert!(edit_toml_text("not = = toml", &[]).is_err());
        assert!(edit_toml_text("[a]\nb = 1\n", &[ch("a.b", "c", json!(1))]).is_err());
        assert!(edit_toml_text("", &[ch("a", "c", serde_json::Value::Null)]).is_err());
    }

    #[test]
    fn env_edit_touches_only_changed_lines() {
        let src = "# secrets\nJWT_SECRET=abc\nexport OPENROUTER_API_KEY=old\n\nOTHER='x y'\nOPENROUTER_API_KEY=dup\n";
        let out = edit_env_text(
            src,
            &[
                ("OPENROUTER_API_KEY".into(), Some("sk-new".into())),
                ("SILOFORGE_BACKUP_PASSPHRASE".into(), Some("p w\"q".into())),
                ("GONE".into(), None),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "# secrets\nJWT_SECRET=abc\nOPENROUTER_API_KEY=sk-new\n\nOTHER='x y'\nSILOFORGE_BACKUP_PASSPHRASE=\"p wq\"\n"
        );
        let removed = edit_env_text(&out, &[("OPENROUTER_API_KEY".into(), None)]).unwrap();
        assert!(!removed.contains("OPENROUTER_API_KEY"));
        assert!(removed.contains("JWT_SECRET=abc"));
        assert!(edit_env_text("", &[("bad key".into(), Some("x".into()))]).is_err());
        assert!(edit_env_text("", &[("K".into(), Some("a\nb".into()))]).is_err());
        let parsed = crate::config::parse_env_file(&out);
        assert!(
            parsed
                .iter()
                .any(|(k, v)| k == "SILOFORGE_BACKUP_PASSPHRASE" && v == "p wq")
        );
    }
}
