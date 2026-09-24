//! The Space's draft vocabulary as its owner reviews it (Spec §20.16).
//!
//! Formation drafts symbols into `kip://local/draft@0.0.0` and Maintenance
//! reviews them; neither can promote one. Promotion is a Schema migration
//! under `manage_schema`, which in this deployment is the Space's management
//! credential: it maps a draft symbol's lineage onto a symbol of the same kind
//! in an installed package, so elements written under the draft are matched as
//! the target from then on while keeping their exact references.
use super::*;
use crate::types::{DraftSymbol, PromoteDraftInput, PromoteDraftOutput, SchemaDrafts};
use anda_cognitive_nexus::schema::SymbolKind;
use serde_json::Value as Json;

impl Space {
    /// Every symbol this Space drafted, with its definition and, once
    /// promoted, the lineage it was promoted to.
    pub async fn schema_drafts(&self) -> Result<SchemaDrafts, BoxError> {
        let environment = self
            .schema_read(r#"DESCRIBE SCHEMA ENVIRONMENT"#)
            .await?
            .ok_or("DESCRIBE SCHEMA ENVIRONMENT returned nothing")?;
        let promoted = |kind: &str, name: &str| -> Option<String> {
            let from = format!("{}/{name}", anda_kip::DRAFT_PACKAGE_ID);
            environment["lineage_maps"]
                .as_array()?
                .iter()
                .find(|map| map["kind"] == kind && map["from"] == from.as_str())
                .and_then(|map| map["to"].as_str())
                .map(str::to_string)
        };
        let mut drafts = SchemaDrafts {
            package_ref: anda_kip::DRAFT_PACKAGE_REF.to_string(),
            schema_environment_version: environment["version"].as_u64().unwrap_or_default(),
            symbols: Vec::new(),
        };
        // Before the first DEFINE the Space has no draft package at all.
        let Some(package) = self
            .schema_read(&format!(
                r#"DESCRIBE PACKAGE "{}""#,
                anda_kip::DRAFT_PACKAGE_REF
            ))
            .await
            .ok()
            .flatten()
        else {
            return Ok(drafts);
        };
        for (section, kind) in [
            ("concept_types", "ConceptType"),
            ("predicates", "PredicateType"),
        ] {
            let Some(definitions) = package["definitions"][section].as_object() else {
                continue;
            };
            for (name, definition) in definitions {
                drafts.symbols.push(DraftSymbol {
                    kind: kind.to_string(),
                    name: name.clone(),
                    reference: anda_kip::draft_symbol_ref(name),
                    definition: definition.clone(),
                    promoted_to: promoted(kind, name),
                });
            }
        }
        Ok(drafts)
    }

    /// Promotes one draft symbol onto an installed package's symbol of the
    /// same kind (Spec §20.16). The caller is the Space's owner; nothing ever
    /// promotes implicitly, and a draft is promoted at most once.
    pub async fn promote_draft_symbol(
        &self,
        input: PromoteDraftInput,
    ) -> Result<PromoteDraftOutput, BoxError> {
        let kind = match input.kind.as_str() {
            "ConceptType" => SymbolKind::ConceptType,
            "PredicateType" => SymbolKind::PredicateType,
            other => {
                return Err(
                    format!("kind must be ConceptType or PredicateType, got {other:?}").into(),
                );
            }
        };
        if input.from.trim().is_empty() || input.to.trim().is_empty() {
            return Err("from and to are required".into());
        }
        let version = self
            .memory
            .nexus()
            .system_session()
            .promote_draft_symbol(
                anda_cognitive_nexus::nexus::DEFAULT_SPACE,
                kind,
                &input.from,
                &input.to,
            )
            .await?;
        let name = input
            .from
            .strip_prefix(&format!("{}/", anda_kip::DRAFT_PACKAGE_REF))
            .unwrap_or(&input.from);
        Ok(PromoteDraftOutput {
            promoted: anda_kip::draft_symbol_ref(name),
            to: input.to,
            schema_environment_version: version,
        })
    }

    async fn schema_read(&self, command: &str) -> Result<Option<Json>, BoxError> {
        let response = self.execute_kip_readonly(kip::request(command)).await?;
        if !kip::succeeded(&response) {
            return Err(kip::error_message(&response).into());
        }
        Ok(kip::ok_result(&response).cloned())
    }
}
