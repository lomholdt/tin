//! `tin-core` — a full-text index whose postings are Postgres ctids stored as
//! two-level bitmaps.
//!
//! This is Phase 0 of our reverse-engineering of PlanetScale's TIN: the
//! storage format and boolean query engine, standalone (no Postgres yet).
//! See `docs/DESIGN.md` for what is known from the TIN posts vs. inferred.
//!
//! ```
//! use tin_core::{Index, Plan, Tid, Analyzer};
//!
//! let docs = vec![
//!     (Tid::new(0, 1), "stretch denim jeans"),
//!     (Tid::new(0, 2), "raw denim jacket"),
//!     (Tid::new(7, 1), "stretch chinos"),
//! ];
//! let index = Index::build(&docs, 2);
//! let plan = Plan::parse("denim -jacket", &mut Analyzer::new()).unwrap();
//! assert_eq!(index.search_vec(&plan), vec![Tid::new(0, 1)]);
//! assert_eq!(index.count(&Plan::parse("stretch", &mut Analyzer::new()).unwrap()), 2);
//! ```

pub mod bitmap;
pub mod cursor;
pub mod index;
pub mod postings;
pub mod query;
pub mod segment;
pub mod tid;
pub mod tokenize;
mod varint;

pub use index::Index;
pub use query::{Plan, Query, QueryError};
pub use segment::{Segment, SegmentBuilder};
pub use tid::Tid;
pub use tokenize::Analyzer;
