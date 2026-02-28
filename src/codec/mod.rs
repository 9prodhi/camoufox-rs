//! Wire-format codec for the Juggler pipe protocol.
//!
//! The codec handles framing (null-byte delimited UTF-8 JSON) and is agnostic
//! to higher-level protocol semantics.

pub mod errors;
pub mod nul_json;

// Re-export the primary types at the module level for convenience.
pub use errors::CodecError;
pub use nul_json::NulJsonCodec;
