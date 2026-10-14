//! Domain enums shared across crates. Text enums carry `Unknown(String)` so unexpected stored values
//! never fail a read. Wire/DB strings match v1 exactly.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

macro_rules! text_enum {
    ($(#[$m:meta])* $name:ident { $($var:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name { $($var,)+ Unknown(String) }

        impl $name {
            pub const ALL: &'static [&'static str] = &[$($s),+];
            pub fn as_str(&self) -> &str {
                match self { $(Self::$var => $s,)+ Self::Unknown(s) => s.as_str() }
            }
            pub fn parse(s: &str) -> Self {
                match s { $($s => Self::$var,)+ other => Self::Unknown(other.to_string()) }
            }
            /// Strict parse for request validation.
            pub fn parse_known(s: &str) -> Option<Self> {
                match Self::parse(s) { Self::Unknown(_) => None, v => Some(v) }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> { s.serialize_str(self.as_str()) }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Ok(Self::parse(&s))
            }
        }
    };
}

text_enum!(Role { Admin => "admin", User => "user", Site => "site" });
text_enum!(AnchorType {
    Exact => "exact", Partial => "partial", Branded => "branded", Generic => "generic",
    Lsi => "lsi", Naked => "naked", Cta => "cta",
});
text_enum!(Topology {
    Reverse => "reverse", Serial => "serial", Priority => "priority", Circle => "circle",
    Bidirectional => "bidirectional",
});
text_enum!(EdgeStatus {
    Suggested => "suggested", Approved => "approved", Rejected => "rejected",
    Applied => "applied", Reverted => "reverted", Failed => "failed",
});
text_enum!(InsertModule { Native => "native", Rephrase => "rephrase", Context => "context", ContentGap => "content_gap" });
text_enum!(JobKind {
    Crawl => "crawl", Plan => "plan", Inject => "inject", Undo => "undo", KeywordLink => "keyword_link",
    LinkHealth => "link_health", Audit => "audit", Embed => "embed", Backup => "backup", Maintenance => "maintenance",
    Gsc => "gsc", LinkRun => "link_run",
});
text_enum!(JobStatus { Queued => "queued", Running => "running", Done => "done", Failed => "failed", Cancelled => "cancelled" });

impl JobKind {
    /// Kinds that mutate a site's data and must never overlap on the same site.
    pub fn is_site_exclusive(&self) -> bool {
        matches!(
            self,
            Self::Crawl
                | Self::Plan
                | Self::Inject
                | Self::Undo
                | Self::KeywordLink
                | Self::Audit
                | Self::Embed
                | Self::Gsc
                | Self::LinkRun
        )
    }
    /// Kinds that use a lot of CPU (respect the CPU budget / defer rules).
    pub fn is_heavy(&self) -> bool {
        matches!(self, Self::Plan | Self::Audit | Self::Embed)
    }
}

/// f32 vector -> little-endian BLOB.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Little-endian BLOB -> f32 vector. Returns None for a malformed length.
pub fn blob_to_vec(b: &[u8]) -> Option<Vec<f32>> {
    if !b.len().is_multiple_of(4) {
        return None;
    }
    Some(
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// Parses pgvector text `[0.1,0.2,...]` (used by the v1 migrator).
pub fn parse_pgvector(s: &str) -> Option<Vec<f32>> {
    let inner = s.trim().strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|x| x.trim().parse::<f32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_roundtrip_and_unknown() {
        assert_eq!(AnchorType::parse("cta"), AnchorType::Cta);
        assert_eq!(AnchorType::parse("weird").as_str(), "weird");
        assert!(AnchorType::parse_known("weird").is_none());
        let j = serde_json::to_string(&Topology::Bidirectional).unwrap();
        assert_eq!(j, "\"bidirectional\"");
        let t: Topology = serde_json::from_str("\"zigzag\"").unwrap();
        assert_eq!(t, Topology::Unknown("zigzag".into()));
    }

    #[test]
    fn blobs() {
        let v = vec![0.5f32, -1.25, 3.0];
        assert_eq!(blob_to_vec(&vec_to_blob(&v)).unwrap(), v);
        assert!(blob_to_vec(&[1, 2, 3]).is_none());
        assert_eq!(parse_pgvector("[0.5,-1.25,3]").unwrap(), v);
        assert!(parse_pgvector("0.5,1").is_none());
        assert!(parse_pgvector("[a,b]").is_none());
    }
}
