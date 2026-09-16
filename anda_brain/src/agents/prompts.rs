//! Immutable agent prompt configuration owned by a host instance.
//!
//! Defaults are compiled in. A trusted launcher may replace a deployment's
//! section A before creating an AppState; the vendored KIP reference prefix
//! always comes from the compiled asset. No process-wide mutable layer exists.

use anda_core::BoxError;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

/// Compiled-in default prompts.
///
/// Each is one mode's policy and this deployment's contract — the KIP language
/// itself is not in here. `anda_kip` ships the syntax card and the Cognitive
/// Memory Profile alongside the protocol they describe, and
/// [`mode_reference`] loads the applicable role cards and Profile at completion time.
/// KIP 1.x taught the language from a copy pasted into each of these files;
/// three hand-maintained copies of a protocol drift, and the one that drifts
/// silently is the one a model then writes against.
pub const FORMATION_DEFAULT: &str = include_str!("../../assets/BrainFormation.md");
pub const RECALL_DEFAULT: &str = include_str!("../../assets/BrainRecall.md");
pub const MAINTENANCE_DEFAULT: &str = include_str!("../../assets/BrainMaintenance.md");

/// The complete reference for callers that need it. Routine agent context uses
/// mode_reference; writing agents can request full syntax on demand.
pub fn language_reference() -> &'static str {
    static REFERENCE: OnceLock<String> = OnceLock::new();
    REFERENCE.get_or_init(|| {
        format!(
            "{}\n\n---\n\n{}",
            anda_kip::KIP_SYNTAX,
            anda_kip::COGNITIVE_MEMORY_PROFILE
        )
    })
}

/// The applicable role cards and ontology; complete syntax is loaded on demand.
pub fn mode_reference(target: PromptTarget) -> String {
    let cards = match target {
        PromptTarget::Formation => anda_kip::KIP_FORMATION_CARD.to_string(),
        PromptTarget::Recall => anda_kip::KIP_RECALL_CARD.to_string(),
        PromptTarget::Maintenance => format!(
            "{}\n\n{}\n\n{}",
            anda_kip::KIP_RECALL_CARD,
            anda_kip::KIP_FORMATION_CARD,
            anda_kip::KIP_MAINTENANCE_CARD
        ),
    };
    format!(
        "{}\n\n{}\n\n{}",
        cards,
        anda_kip::COGNITIVE_MEMORY_PROFILE,
        crate::cognitive::CAPABILITIES
    )
}

/// Which agent receives a deployment-specific configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PromptTarget {
    Formation,
    #[default]
    Recall,
    Maintenance,
}

impl PromptTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Formation => "formation",
            Self::Recall => "recall",
            Self::Maintenance => "maintenance",
        }
    }

    pub fn default_prompt(self) -> &'static str {
        match self {
            Self::Formation => FORMATION_DEFAULT,
            Self::Recall => RECALL_DEFAULT,
            Self::Maintenance => MAINTENANCE_DEFAULT,
        }
    }
}

/// Shared immutable compiled defaults. Retained as the default-prompt getter
/// for existing integrations; it can no longer observe a global override.
static DEFAULTS: [OnceLock<Arc<str>>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];

fn slot(target: PromptTarget) -> usize {
    match target {
        PromptTarget::Formation => 0,
        PromptTarget::Recall => 1,
        PromptTarget::Maintenance => 2,
    }
}

pub fn active_prompt(target: PromptTarget) -> Arc<str> {
    DEFAULTS[slot(target)]
        .get_or_init(|| Arc::from(target.default_prompt()))
        .clone()
}

/// Trusted, immutable instance configuration. Only the deployment section can
/// be supplied by the host; protocol policy is always the compiled reference.
/// This is Rust configuration, never an agent tool or a public HTTP payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPrompts([Arc<str>; 3]);

impl Default for AgentPrompts {
    fn default() -> Self {
        Self([
            active_prompt(PromptTarget::Formation),
            active_prompt(PromptTarget::Recall),
            active_prompt(PromptTarget::Maintenance),
        ])
    }
}

impl AgentPrompts {
    /// Replaces only the selected deployment's section A in this value.
    /// Other instances and the compiled default getter remain unchanged.
    pub fn with_deployment_section(
        mut self,
        target: PromptTarget,
        section: &str,
    ) -> Result<Self, BoxError> {
        if !section.starts_with("# A.") || section.len() > 128 * 1024 {
            return Err("deployment prompt must start with # A. and fit within 128 KiB".into());
        }
        let (reference, _) = target
            .default_prompt()
            .split_once("\n# A.")
            .ok_or("compiled prompt is missing its deployment boundary")?;
        self.0[slot(target)] = Arc::from(format!("{reference}\n{section}"));
        Ok(self)
    }

    pub fn prompt(&self, target: PromptTarget) -> Arc<str> {
        self.0[slot(target)].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_prompt_edits_preserve_compiled_reference_and_other_instances() {
        let baseline = AgentPrompts::default();
        let changed = baseline
            .clone()
            .with_deployment_section(PromptTarget::Recall, "# A. local\nINSTANCE_ONLY")
            .unwrap();
        let reference = RECALL_DEFAULT.split_once("\n# A.").unwrap().0;
        assert_eq!(
            changed
                .prompt(PromptTarget::Recall)
                .split_once("\n# A.")
                .unwrap()
                .0,
            reference
        );
        assert!(
            changed
                .prompt(PromptTarget::Recall)
                .ends_with("INSTANCE_ONLY")
        );
        assert_eq!(
            baseline.prompt(PromptTarget::Recall).as_ref(),
            RECALL_DEFAULT
        );
        assert_eq!(active_prompt(PromptTarget::Recall).as_ref(), RECALL_DEFAULT);
        assert_eq!(
            changed.prompt(PromptTarget::Formation),
            baseline.prompt(PromptTarget::Formation)
        );
        assert!(
            baseline
                .clone()
                .with_deployment_section(PromptTarget::Recall, "replace reference policy")
                .is_err()
        );
        assert!(
            baseline
                .with_deployment_section(
                    PromptTarget::Recall,
                    &format!("# A.{}", "x".repeat(128 * 1024))
                )
                .is_err()
        );
    }
}
