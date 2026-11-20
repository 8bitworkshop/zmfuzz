//! `zmfuzz`: a shortest-path-to-every-atom fuzzer for Z-machine story files.
//!
//! A port of `search.js` (from zzo38's JSZM) on top of `encrusted-heart`.
//! Point it at a story file and it wanders around, recording which commands
//! first produced which output, then aims at the tokens it knows least about.
//! Progress is measured in *tokens*: printed text, plus `@mv`/`@pp`/`@f`
//! VM-state changes recorded by the VM.

pub mod checkpoint;
pub mod parallel;
pub mod rng;
pub mod search;
pub mod stat;
pub mod token;
pub mod tui;
pub mod ui;
