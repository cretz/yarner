//! The yarner engine: a story graph of beats the LLM writes, generated ahead of every
//! player, with playthroughs, versioned prompts and edits.
//!
//! - [`model`]: records and the graph they fold into.
//! - [`contract`]: what the LLM returns, its schema, the engine prompt, tools.
//! - [`context`]: what the LLM sees for one generation.
//! - [`backend`]: the generator interface (thinfer-serve or a mock).
//! - [`engine`]: scheduling, generation, playthroughs, edits.
//! - [`plays`], [`view`]: game state and client views.

pub mod backend;
pub mod context;
pub mod contract;
#[cfg(feature = "runtime")]
pub mod engine;
pub mod model;
pub mod plays;
pub mod shot;
pub mod view;

#[cfg(feature = "runtime")]
pub use engine::{Advance, Config, Engine, EngineError, Event, TextStat, TextSummary};
