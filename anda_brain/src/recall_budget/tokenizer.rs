//! The encoding is a protocol choice, not inferred from a provider/model name.

use anda_core::BoxError;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::OnceLock,
};

pub const TOKENIZER: &str = "o200k_base@tiktoken-rs-0.12.0";

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
