//! yarner's encrypted storage: nothing a story holds is ever on disk in the clear.
//!
//! - [`crypto`]: keys, the passphrase KDF, subkeys, small sealed values.
//! - [`blob`]: chunked sealed files with random access (media).
//! - [`log`]: append-only sealed record logs (graph, plays).
//! - [`store`]: an install's layout, unlocking stories by passphrase.
//! - [`bundle`]: export zips and reading them back.

pub mod blob;
pub mod bundle;
pub mod crypto;
pub mod log;
pub mod store;

pub use crypto::{KdfParams, Key};
pub use store::{Install, Story};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Wrong key or tampered data.
    #[error("decryption failed")]
    Decrypt,
    #[error("corrupt data: {0}")]
    Corrupt(&'static str),
}
