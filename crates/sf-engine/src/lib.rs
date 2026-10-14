//! SiloForge linking engine: pure, deterministic algorithms with no IO.
//!
//! Text side (WP E1): `html`, `nlp`, `anchor`, `insert`, `linker`.
//! Graph side (WP E2): `graph`, `cluster`, `silo`, `score`, `density`, `vector`, `metrics`, `pool`.
//! Spec: `docs/research/01-engine-spec.md`.

pub mod anchor;
pub mod html;
pub mod insert;
pub mod linker;
pub mod nlp;

pub mod cluster;
pub mod density;
pub mod graph;
pub mod metrics;
pub mod pool;
pub mod score;
pub mod silo;
pub mod vector;
