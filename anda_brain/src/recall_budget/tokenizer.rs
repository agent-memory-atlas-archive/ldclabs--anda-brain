//! The encoding is a protocol choice, not inferred from a provider/model name.

use anda_core::BoxError;
use serde::{Deserialize, Deserializer};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::OnceLock,
};

/// The encoding Recall counts with: `o200k_base` as the tiktoken-rs 0.12 line
/// implements it. `Cargo.toml` admits patch releases of that line only; a
/// release that changes the encoding needs a new identity.
pub const TOKENIZER: &str = "o200k_base@tiktoken-rs-0.12";

/// The identity Brain advertised through 0.13.1, naming the patch release.
/// It is the same encoding, so stored policies and clients that still send it
/// are accepted and read as [`TOKENIZER`].
pub const LEGACY_TOKENIZER: &str = "o200k_base@tiktoken-rs-0.12.0";

/// The current identity of a supported tokenizer name, `None` otherwise.
pub fn canonical_tokenizer(name: &str) -> Option<&'static str> {
    matches!(name, TOKENIZER | LEGACY_TOKENIZER).then_some(TOKENIZER)
}

/// Reads a tokenizer name, rewriting a supported one to [`TOKENIZER`]; any
/// other name is kept for validation to refuse.
pub(super) fn deserialize_tokenizer<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let name = String::deserialize(deserializer)?;
    Ok(canonical_tokenizer(&name).map_or(name, str::to_string))
}

static ENCODING: OnceLock<Result<tiktoken_rs::CoreBPE, String>> = OnceLock::new();

/// Count the actual string as ordinary text, including strings that look like
/// special-token markers. Initialization failure is cached and returned; it
/// never falls back to a heuristic or a different encoding.
pub fn count(text: &str) -> Result<usize, BoxError> {
    match ENCODING.get_or_init(|| {
        catch_unwind(tiktoken_rs::o200k_base)
            .map_err(|_| "pinned tokenizer initialization panicked".to_string())
            .and_then(|result| result.map_err(|error| error.to_string()))
    }) {
        // This version's ordinary encoder unwraps regex matching errors. Do
        // not let one such failure turn into an unchecked estimate or a
        // partially delivered packet. The shared codec only owns token tables
        // and internal regex caches; no host state is mutated by this call.
        Ok(encoding) => catch_unwind(AssertUnwindSafe(|| encoding.encode_ordinary(text).len()))
            .map_err(|_| "Recall tokenizer failed to encode ordinary text".into()),
        Err(error) => Err(format!("Recall tokenizer initialization failed: {error}").into()),
    }
}
