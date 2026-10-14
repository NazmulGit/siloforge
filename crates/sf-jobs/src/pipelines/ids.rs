//! Row ids for bulk inserts.
//!
//! `sf_core::new_id()` is a random UUIDv4. Inserting tens of thousands of random TEXT primary keys
//! scatters writes over the whole PK index; on the 1.8 GB migrated DB that made 118k keyword
//! inserts take 8.7 s vs 1.7 s with time-ordered ids (docs/testing/performance.md). `row_id()`
//! returns a UUIDv7 (48-bit Unix ms timestamp + the v4's random bits), so new rows land at the end
//! of the index. Still a valid, unique UUID string (v1 Postgres `uuid` compatible).

/// Time-ordered UUIDv7 string.
pub fn row_id() -> String {
    let v4 = sf_core::new_id();
    let ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0) & 0xFFFF_FFFF_FFFF;
    // v4 layout: xxxxxxxx-xxxx-4xxx-Vxxx-xxxxxxxxxxxx; keep everything after the version nibble.
    match v4.get(15..) {
        Some(tail) if v4.len() == 36 => {
            format!("{:08x}-{:04x}-7{tail}", ms >> 16, ms & 0xFFFF)
        }
        _ => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_id_is_a_time_ordered_v7_uuid() {
        let a = row_id();
        assert_eq!(a.len(), 36);
        assert_eq!(a.get(14..15), Some("7"));
        assert!(matches!(a.get(19..20), Some("8" | "9" | "a" | "b")));
        assert_eq!(
            a.split('-').map(str::len).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = row_id();
        assert!(b > a, "{b} should sort after {a}");
        assert_ne!(row_id(), row_id());
    }
}
