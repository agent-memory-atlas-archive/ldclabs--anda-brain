//! Opt-in, host-owned Recall selection and serialization budgets.
//!
//! This package is deliberately not a completeness or execution permit. The
//! host filters authorization and assigns priorities before calling [`pack`];
//! model selection can only choose among those existing items.

mod tokenizer;

pub use tokenizer::{TOKENIZER, count};

use anda_core::{BoxError, Json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_ITEMS: usize = 256;
pub const MAX_ITEM_CONTENT_BYTES: usize = 1024 * 1024;
pub const PACKET_FORMAT: &str = "anda-brain-recall/1";

/// Both limits use the explicitly named encoding, independently of the model.
/// Defaults take effect only after the host/request explicitly opts in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "mcp", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
pub struct RecallBudget {
    pub tokenizer: String,
    pub max_tokens: u32,
    pub context_tokens: u32,
}

impl Default for RecallBudget {
    fn default() -> Self {
        Self {
            tokenizer: TOKENIZER.into(),
            max_tokens: 4096,
            context_tokens: 32768,
        }
    }
}

impl RecallBudget {
    pub fn validate(&self) -> Result<(), BoxError> {
        if self.tokenizer != TOKENIZER {
            return Err("unsupported Recall tokenizer".into());
        }
        if !(1..=65_536).contains(&self.max_tokens) {
            return Err("Recall max_tokens must be in 1..=65536".into());
        }
        if !(1..=131_072).contains(&self.context_tokens) {
            return Err("Recall context_tokens must be in 1..=131072".into());
        }
        Ok(())
    }

    /// Request limits may narrow an operator policy, never increase it.
    pub fn resolve(
        policy: Option<&Self>,
        request: Option<&Self>,
    ) -> Result<Option<Self>, BoxError> {
        if let Some(policy) = policy {
            policy.validate()?;
        }
        if let Some(request) = request {
            request.validate()?;
        }
        match (policy, request) {
            (None, None) => Ok(None),
            (Some(one), None) | (None, Some(one)) => Ok(Some(one.clone())),
            (Some(policy), Some(request)) => {
                if policy.tokenizer != request.tokenizer {
                    return Err("Recall policy and request tokenizers differ".into());
                }
                Ok(Some(Self {
                    tokenizer: policy.tokenizer.clone(),
                    max_tokens: policy.max_tokens.min(request.max_tokens),
                    context_tokens: policy.context_tokens.min(request.context_tokens),
                }))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Primer,
    Notes,
    Counterparty,
    History,
    Kip,
    Wiki,
    Procedures,
}

/// This order is assigned by trusted host code, never accepted from a model.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Required,
    Warning,
    VerifiedProcedure,
    Relevant,
    UnprovenProcedure,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MemoryItem {
    pub id: String,
    pub channel: Channel,
    pub priority: Priority,
    pub content: Json,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Coverage {
    pub queried: Vec<Channel>,
    pub partial: Vec<Channel>,
    pub omitted: Vec<Channel>,
    pub unchecked: Vec<Channel>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MemoryPacket {
    pub format: String,
    pub status: String,
    /// Static host failure code, included only when it fits the packet budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_reason: Option<String>,
    pub tokenizer: String,
    pub token_limit: u32,
    pub items: Vec<MemoryItem>,
    pub coverage: Coverage,
    pub semantic_complete: bool,
    pub action_ready: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerializedPacket {
    /// Compact JSON, including all items and coverage. At budgets too small
    /// for even the failure envelope this is exactly `null`, meaning no usable
    /// memory packet; callers must retain [`Self::insufficient`] out of band.
    pub content: String,
    pub tokens: usize,
    pub insufficient: bool,
}

/// Select whole, already-authorized memory items and enforce the limit against
/// the final compact JSON, including keys, escaping and omission metadata.
///
/// Required constraints and warnings are an indivisible set. Optional items are
/// considered by host priority, then by stable id; the model can select ids but
/// cannot change either order or content. Unselected and oversized items remain
/// visible as omitted channels. No outcome claims semantic completeness, actual
/// use or execution permission.
pub fn pack(
    budget: &RecallBudget,
    items: &[MemoryItem],
    selected_ids: &[String],
    coverage: Coverage,
) -> Result<SerializedPacket, BoxError> {
    pack_ranked(
        budget,
        items,
        selected_ids,
        coverage,
        &std::collections::BTreeMap::new(),
    )
}

/// Optional host-verified utility: signalled items sort first inside their
/// existing priority, then by utility. Required/warning admission is unchanged.
pub fn pack_ranked(
    budget: &RecallBudget,
    items: &[MemoryItem],
    selected_ids: &[String],
    coverage: Coverage,
    utility: &std::collections::BTreeMap<String, f64>,
) -> Result<SerializedPacket, BoxError> {
    budget.validate()?;
    validate_items(items)?;

    let mut ordered: Vec<&MemoryItem> = items.iter().collect();
    ordered.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| {
                let score = |id: &str| {
                    utility
                        .get(id)
                        .copied()
                        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                };
                score(&right.id)
                    .partial_cmp(&score(&left.id))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.id.cmp(&right.id))
    });
    let selected: BTreeSet<&str> = selected_ids.iter().map(String::as_str).collect();
    let mut kept: Vec<&MemoryItem> = ordered
        .iter()
        .filter(|item| item.priority <= Priority::Warning)
        .copied()
        .collect();
    let mut serialized = serialize_packet(budget, items, &kept, &coverage, false)?;
    if serialized.tokens > budget.max_tokens as usize {
        // Nothing may remain executable-looking when necessary constraints or
        // warnings cannot be delivered. The full set is retained in storage;
        // this operation only controls its current delivery.
        return insufficient_packet(budget, items, &coverage);
    }

    for item in ordered {
        if item.priority <= Priority::Warning || !selected.contains(item.id.as_str()) {
            continue;
        }
        kept.push(item);
        // Coverage can grow when a formerly entirely omitted channel becomes
        // partial. Recount the complete candidate instead of subtracting token
        // estimates or reserving a fixed number of metadata tokens.
        let candidate = serialize_packet(budget, items, &kept, &coverage, false)?;
        if candidate.tokens <= budget.max_tokens as usize {
            serialized = candidate;
        } else {
            kept.pop();
        }
    }
    Ok(serialized)
}

/// Fail closed when the host could not complete necessary reads or fit the
/// model context. This contains no remembered content or ordinary answer.
pub fn insufficient(
    budget: &RecallBudget,
    coverage: Coverage,
) -> Result<SerializedPacket, BoxError> {
    budget.validate()?;
    insufficient_packet(budget, &[], &coverage)
}

/// Keep a static host diagnostic inside the same counted delivery boundary.
/// Tiny budgets retain their existing failure envelope or null sentinel.
pub(crate) fn with_failure_reason(
    limits: &RecallBudget,
    packet: SerializedPacket,
    reason: &str,
) -> Result<SerializedPacket, BoxError> {
    debug_assert!(packet.insufficient);
    let Some(mut value) = serde_json::from_str::<Option<MemoryPacket>>(&packet.content)? else {
        return Ok(packet);
    };
    value.failed_reason = Some(reason.to_string());
    let content = serde_json::to_string(&value)?;
    let tokens = count(&content)?;
    if tokens > limits.max_tokens as usize {
        return Ok(packet);
    }
    Ok(SerializedPacket {
        content,
        tokens,
        insufficient: true,
    })
}

fn validate_items(items: &[MemoryItem]) -> Result<(), BoxError> {
    if items.len() > MAX_ITEMS {
        return Err("Recall memory item count exceeds 256".into());
    }
    let mut ids = BTreeSet::new();
    for item in items {
        if item.id.is_empty() || !ids.insert(&item.id) {
            return Err("Recall memory item ids must be nonempty and unique".into());
        }
        if serde_json::to_vec(&item.content)?.len() > MAX_ITEM_CONTENT_BYTES {
            return Err("Recall memory item content exceeds 1 MiB of serialized JSON".into());
        }
    }
    Ok(())
}

fn normalized(mut coverage: Coverage) -> Coverage {
    for channels in [
        &mut coverage.queried,
        &mut coverage.partial,
        &mut coverage.omitted,
        &mut coverage.unchecked,
    ] {
        channels.sort_unstable();
        channels.dedup();
    }
    coverage
}

fn delivery_coverage(items: &[MemoryItem], kept: &[&MemoryItem], original: &Coverage) -> Coverage {
    let kept_ids: BTreeSet<&str> = kept.iter().map(|item| item.id.as_str()).collect();
    let kept_channels: BTreeSet<Channel> = kept.iter().map(|item| item.channel).collect();
    let mut coverage = original.clone();
    for item in items {
        if !kept_ids.contains(item.id.as_str()) {
            coverage.omitted.push(item.channel);
            if kept_channels.contains(&item.channel) {
                coverage.partial.push(item.channel);
            }
        }
    }
    // A host-reported omission remains true even if some new content from the
    // same channel fits. Never remove its history during selection.
    for channel in &coverage.omitted {
        if kept_channels.contains(channel) {
            coverage.partial.push(*channel);
        }
    }
    normalized(coverage)
}

fn serialize_packet(
    budget: &RecallBudget,
    items: &[MemoryItem],
    kept: &[&MemoryItem],
    coverage: &Coverage,
    insufficient: bool,
) -> Result<SerializedPacket, BoxError> {
    // Borrow the selected JSON instead of deep-cloning it at every admission.
    // Field order matches MemoryPacket so the token boundary is unchanged.
    #[derive(Serialize)]
    struct PacketView<'a> {
        format: &'static str,
        status: &'static str,
        tokenizer: &'static str,
        token_limit: u32,
        items: &'a [&'a MemoryItem],
        coverage: Coverage,
        semantic_complete: bool,
        action_ready: bool,
    }
    let packet = PacketView {
        format: PACKET_FORMAT,
        status: if insufficient {
            "budget_insufficient"
        } else {
            "bounded"
        },
        tokenizer: TOKENIZER,
        token_limit: budget.max_tokens,
        items: kept,
        coverage: delivery_coverage(items, kept, coverage),
        semantic_complete: false,
        action_ready: false,
    };
    let content = serde_json::to_string(&packet)?;
    let tokens = count(&content)?;
    Ok(SerializedPacket {
        content,
        tokens,
        insufficient,
    })
}

fn insufficient_packet(
    budget: &RecallBudget,
    items: &[MemoryItem],
    coverage: &Coverage,
) -> Result<SerializedPacket, BoxError> {
    let packet = serialize_packet(budget, items, &[], coverage, true)?;
    if packet.tokens <= budget.max_tokens as usize {
        return Ok(packet);
    }
    let content = "null".to_string();
    let tokens = count(&content)?;
    if tokens > budget.max_tokens as usize {
        return Err("Recall budget cannot encode the null failure sentinel".into());
    }
    Ok(SerializedPacket {
        content,
        tokens,
        insufficient: true,
    })
}

#[cfg(test)]
mod tests;
