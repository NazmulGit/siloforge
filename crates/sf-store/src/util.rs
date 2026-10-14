//! Small helpers for row mapping.

use rusqlite::Row;
use rusqlite::types::ValueRef;
use serde_json::Value;

/// Reads a TEXT column holding JSON; missing/invalid -> `default`.
pub fn json_col(row: &Row<'_>, idx: &str, default: Value) -> Value {
    match row.get_ref(idx) {
        Ok(ValueRef::Text(t)) => serde_json::from_slice(t).unwrap_or(default),
        _ => default,
    }
}

/// Reads an INTEGER 0/1 column as bool (NULL -> false).
pub fn bool_col(row: &Row<'_>, idx: &str) -> bool {
    matches!(row.get_ref(idx), Ok(ValueRef::Integer(i)) if i != 0)
}

/// Converts one SQLite row into a JSON object (column name -> value). TEXT columns whose name is in
/// `json_cols` are parsed as JSON; INTEGER columns in `bool_cols` become booleans; BLOBs are skipped.
pub fn row_to_json(row: &Row<'_>, json_cols: &[&str], bool_cols: &[&str]) -> Value {
    let stmt = row.as_ref();
    let mut map = serde_json::Map::with_capacity(stmt.column_count());
    for i in 0..stmt.column_count() {
        let Ok(name) = stmt.column_name(i) else {
            continue;
        };
        let v = match row.get_ref(i) {
            Ok(ValueRef::Null) => Value::Null,
            Ok(ValueRef::Integer(n)) => {
                if bool_cols.contains(&name) {
                    Value::Bool(n != 0)
                } else {
                    Value::from(n)
                }
            }
            Ok(ValueRef::Real(f)) => serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Ok(ValueRef::Text(t)) => {
                let s = String::from_utf8_lossy(t);
                if json_cols.contains(&name) {
                    serde_json::from_str(&s).unwrap_or(Value::Null)
                } else {
                    Value::String(s.into_owned())
                }
            }
            Ok(ValueRef::Blob(_)) | Err(_) => continue,
        };
        map.insert(name.to_string(), v);
    }
    Value::Object(map)
}

/// Builds `?,?,?` for IN-lists.
pub fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}
