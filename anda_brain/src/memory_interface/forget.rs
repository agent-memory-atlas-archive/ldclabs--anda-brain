//! forget (MI §4): a bounded, governed ErasurePlan (Spec §60.7) over an
//! exact target, and a report of what was actually erased.
//!
//! `payload_only` purges an Evidence payload (or a staged source's bytes and
//! the Evidence captured from it) and keeps the element. `semantic` erases the
//! memory and what was formed from it: a claim goes through the product
//! deletion path — its tuple, every Assertion on it, their Evidence and their
//! dependents, with the sources suppressed so Formation never re-ingests
//! them — and the host then scrubs its own copies: staged source bytes,
//! Formation transcripts, Recall transcripts that quoted the erased elements,
//! usage-ledger rows and the probe miss cache. `completed` is reported only
//! after the Nexus validated the plan's element and payload surfaces and the
//! host verified its own; a legal hold is `blocked`; a target whose sources
//! cannot be enumerated is `partial`. A semantic forget is the owner's
//! decision (MI §4, single-agent preset): a delegated Space token cannot ask
//! for one.
use super::*;
use crate::product::{ChangeInput, ChangeKind};
use anda_kip::memory::binding::{ForgetInput, ForgetMode, ForgetResult, ForgetStatus};
use object_store::PutMode;
use std::collections::BTreeSet;

/// The ErasurePlan and its coverage, retained with the receipt.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetainedPlan {
    pub plan_ref: String,
    pub namespace: String,
    pub receipt_ref: String,
    /// The ErasurePlan (`kip-cognitive-records.schema.json#/$defs/ErasurePlan`).
    pub plan: Json,
    /// Host surfaces outside the Nexus: `(surface, ref, state)`.
    pub host_surfaces: Vec<(String, String, String)>,
}

fn target(reference: &str, surface: &str, state: &str) -> Json {
    json!({"ref": reference, "surface": surface, "state": state})
}

impl Space {
    /// forget (see the module docs).
    pub(crate) async fn memory_forget(
        self: &Arc<Self>,
        namespace: &str,
        owner: bool,
        request: &Request,
        input: ForgetInput,
    ) -> Result<Response, KipError> {
        if input.mode == ForgetMode::Semantic && !owner {
            return Err(KipError::not_authorized(
                "a semantic forget is the owner's decision; use an owner credential",
            ));
        }
        let key = request
            .idempotency_key
            .as_deref()
            .ok_or_else(|| KipError::invalid_request_envelope("forget needs a key"))?;
        let meaning = anda_cognitive_nexus::content_digest(&json!({
            "operation": request.operation,
            "input": request.input,
        }))?;
        let key_id = anda_cognitive_nexus::content_digest(&json!([
            namespace,
            self.id(),
            request.operation,
            key
        ]))?[7..47]
            .to_string();
        let receipt_ref = format!("rcpt-{key_id}");
        let journal = &self.memory_interface.journal;
        let record_path = intake::receipt_path(&receipt_ref)?;
        let _gate = self.memory_interface.gate.lock().await;
        if let Some(existing) = journal
            .read::<intake::IntakeRecord>(&record_path)
            .await
            .map_err(kip_error)?
        {
            let existing = existing.value;
            if existing.namespace != namespace || existing.intent_digest != meaning {
                return Err(KipError::new(
                    KipErrorCode::IdempotencyConflict,
                    "this idempotency key was used for a different request",
                ));
            }
            drop(_gate);
            return self.forget_response(request, existing).await;
        }
        let record = intake::IntakeRecord {
            receipt: wire::Receipt {
                receipt_ref: receipt_ref.clone(),
                operation: Operation::Forget,
                space_id: self.id().to_string(),
                accepted_seq: self.memory_seq().await?,
            },
            namespace: namespace.to_string(),
            intent_digest: meaning,
            scope: ResolvedScope::default(),
            source_ref: None,
            source_digest: None,
            conversation: None,
            intent: None,
            terminal: None,
            result: None,
            warnings: vec![],
            created_at: anda_engine::unix_ms(),
        };
        journal
            .create(&record_path, &record)
            .await
            .map_err(kip_error)?;
        drop(_gate);
        // The erasure runs as durable host work: a dropped request never
        // leaves a purge half-applied, and the receipt reports what it did.
        let this = self.clone();
        let namespace = namespace.to_string();
        let run = self
            .memory_interface
            .tasks
            .run(async move {
                let outcome = this.erase(&namespace, &receipt_ref, &input).await;
                this.settle_forget(&receipt_ref, outcome)
                    .await
                    .map_err(Into::into)
            })
            .await
            .map_err(kip_error)?;
        self.forget_response(request, run).await
    }

    async fn forget_response(
        self: &Arc<Self>,
        request: &Request,
        record: intake::IntakeRecord,
    ) -> Result<Response, KipError> {
        let progress = self.progress_of(&record).await?;
        let result = record.result.clone().unwrap_or_else(|| {
            json!(ForgetResult {
                status: ForgetStatus::Pending,
                plan_ref: plan_ref(&record.receipt.receipt_ref),
                summary: "Erasure is in progress.".into(),
                coverage_ref: plan_ref(&record.receipt.receipt_ref),
            })
        });
        let partial = result["status"] == "partial";
        let response = mutation_response(
            request,
            record.receipt.clone(),
            progress,
            result,
            record.warnings.clone(),
        );
        Ok(if partial && response.status == Status::Pending {
            with_status(response, Status::Partial)
        } else {
            response
        })
    }

    /// The ErasurePlan behind a forget, for its owner.
    pub async fn memory_plan(&self, namespace: &str, plan: &str) -> Result<Json, KipError> {
        let id = plan
            .strip_prefix("plan-")
            .ok_or_else(|| KipError::not_found_or_not_visible("plan not found"))?;
        let retained = self
            .memory_interface
            .journal
            .read::<RetainedPlan>(&format!("plans/{id}"))
            .await
            .map_err(kip_error)?
            .ok_or_else(|| KipError::not_found_or_not_visible("plan not found"))?
            .value;
        if retained.namespace != namespace {
            return Err(KipError::not_found_or_not_visible("plan not found"));
        }
        Ok(json!({
            "plan_ref": retained.plan_ref,
            "receipt_ref": retained.receipt_ref,
            "plan": retained.plan,
            "host_surfaces": retained.host_surfaces.iter().map(|(surface, reference, state)| {
                json!({"surface": surface, "ref": reference, "state": state})
            }).collect::<Vec<_>>(),
        }))
    }

    async fn settle_forget(
        &self,
        receipt_ref: &str,
        outcome: Result<(ForgetStatus, String, u64), KipError>,
    ) -> Result<intake::IntakeRecord, KipError> {
        let journal = &self.memory_interface.journal;
        let path = intake::receipt_path(receipt_ref)?;
        loop {
            let stored = journal
                .read::<intake::IntakeRecord>(&path)
                .await
                .map_err(kip_error)?
                .ok_or_else(|| KipError::internal_error("receipt vanished"))?;
            let mut record = stored.value;
            match &outcome {
                Ok((status, summary, seq)) => {
                    record.result = Some(json!(ForgetResult {
                        status: *status,
                        plan_ref: plan_ref(receipt_ref),
                        summary: summary.clone(),
                        coverage_ref: plan_ref(receipt_ref),
                    }));
                    record.terminal = match status {
                        ForgetStatus::Completed => Some(wire::Progress {
                            receipt_ref: receipt_ref.to_string(),
                            phase: wire::Phase::Available,
                            disposition: Some(wire::Disposition::Erased),
                            resolved_seq: Some(*seq),
                            available_seq: Some(*seq),
                            reason: None,
                            error: None,
                        }),
                        ForgetStatus::Blocked => Some(wire::Progress {
                            receipt_ref: receipt_ref.to_string(),
                            phase: wire::Phase::Failed,
                            disposition: None,
                            resolved_seq: None,
                            available_seq: None,
                            reason: Some(summary.clone()),
                            error: Some(error_object(&KipError::new(
                                KipErrorCode::LegalHoldConflict,
                                summary.clone(),
                            ))),
                        }),
                        _ => None,
                    };
                    if *status == ForgetStatus::Partial {
                        let note = format!("partial: {summary}");
                        if !record.warnings.contains(&note) {
                            record.warnings.push(note);
                        }
                    }
                }
                Err(error) => {
                    record.terminal = Some(wire::Progress {
                        receipt_ref: receipt_ref.to_string(),
                        phase: wire::Phase::Failed,
                        disposition: None,
                        resolved_seq: None,
                        available_seq: None,
                        reason: Some("the erasure could not run".into()),
                        error: Some(error_object(error)),
                    });
                }
            }
            if journal
                .put(&path, &record, PutMode::Update(stored.version))
                .await
                .is_ok()
            {
                return Ok(record);
            }
        }
    }

    /// Runs the plan and returns its status, a summary and the sequence the
    /// last erasure committed at.
    async fn erase(
        self: &Arc<Self>,
        namespace: &str,
        receipt_ref: &str,
        input: &ForgetInput,
    ) -> Result<(ForgetStatus, String, u64), KipError> {
        let nexus = self.memory.nexus();
        let mut nexus_targets: Vec<Json> = Vec::new();
        let mut host_surfaces: Vec<(String, String, String)> = Vec::new();
        let mut source_roots: Vec<String> = Vec::new();
        let mut erased_ids: BTreeSet<String> = BTreeSet::new();
        let mut suppressed: BTreeSet<String> = BTreeSet::new();
        let mut partial: Option<String> = None;
        let target_ref = input.target_ref.as_str();

        // The staged source a target names, and the Evidence captured from it.
        let mut evidence_targets: Vec<String> = Vec::new();
        let mut staged: Option<String> = None;
        if target_ref.starts_with("src-") {
            let source = self.staged_memory_source(namespace, target_ref).await?;
            staged = Some(source.source_ref.clone());
            suppressed.insert(format!("memory-source:{}", source.source_ref));
            suppressed.insert(format!("memory-source-digest:{}", source.source_digest));
            evidence_targets.extend(self.evidence_of_source(&source.source_ref).await?);
        } else {
            let id: ElementId = target_ref
                .parse()
                .map_err(|_| KipError::not_found_or_not_visible("forget target not found"))?;
            match nexus.store.get_element(id).await {
                Ok(element) if element.space() == DEFAULT_SPACE && element.state() != "purged" => {}
                _ => {
                    return Err(KipError::not_found_or_not_visible(
                        "forget target not found",
                    ));
                }
            }
            if id.kind == anda_kip::ElementKind::Evidence {
                evidence_targets.push(id.to_string());
            }
        }

        match input.mode {
            ForgetMode::PayloadOnly => {
                if evidence_targets.is_empty() && staged.is_none() {
                    return Err(KipError::constraint_violation(
                        "payload_only forgets an Evidence payload or a staged source",
                    ));
                }
                for evidence in &evidence_targets {
                    let response = self
                        .run_kip_settlement(crate::kip::request_with(
                            "PURGE PAYLOAD :id CONFIRM \"PURGE\"",
                            crate::kip::param("id", evidence.as_str()),
                        ))
                        .await
                        .map_err(kip_error)?;
                    if !crate::kip::succeeded(&response) {
                        let error = crate::kip::error_message(&response);
                        if error.contains("LegalHold") || error.contains("legal hold") {
                            return self
                                .finish_plan(
                                    namespace,
                                    receipt_ref,
                                    "payload_only",
                                    nexus_targets,
                                    host_surfaces,
                                    source_roots,
                                    ForgetStatus::Blocked,
                                    format!("a legal hold retains {evidence}"),
                                )
                                .await;
                        }
                        return Err(KipError::internal_error(error));
                    }
                    nexus_targets.push(target(evidence, "payload", "erased"));
                }
            }
            ForgetMode::Semantic => {
                // A claim goes through product deletion; so does any Evidence
                // a claim cites. Evidence nothing cites is purged directly.
                let mut claims: Vec<String> = Vec::new();
                if target_ref.starts_with("A-") {
                    claims.push(target_ref.to_string());
                } else if target_ref.starts_with("P-") {
                    claims.extend(self.assertions_on(target_ref).await?);
                }
                for evidence in &evidence_targets {
                    claims.extend(self.assertions_citing(evidence).await?);
                }
                claims.sort();
                claims.dedup();
                for claim in &claims {
                    match self.product_delete(receipt_ref, claim).await {
                        Ok((targets, sources)) => {
                            for (id, kind) in targets {
                                if kind == "evidence" {
                                    source_roots.push(id.clone());
                                }
                                nexus_targets.push(target(&id, "element", "erased"));
                                erased_ids.insert(id);
                            }
                            suppressed.extend(sources);
                        }
                        Err(error) if error.to_string().contains("unsupported_scope") => {
                            // A held element, or sources that cannot be
                            // verified: nothing of this claim was erased.
                            if self.held(claim).await {
                                return self
                                    .finish_plan(
                                        namespace,
                                        receipt_ref,
                                        "semantic_forgetting",
                                        nexus_targets,
                                        host_surfaces,
                                        source_roots,
                                        ForgetStatus::Blocked,
                                        format!("a legal hold retains {claim} or its closure"),
                                    )
                                    .await;
                            }
                            partial = Some(format!(
                                "{claim}'s sources cannot be verified, so it was not erased"
                            ));
                            nexus_targets.push(target(claim, "element", "pending"));
                        }
                        Err(error) => return Err(kip_error(error)),
                    }
                }
                // Anything the product deletion did not reach: the target
                // itself, or uncited Evidence.
                let mut direct: Vec<String> = evidence_targets
                    .iter()
                    .filter(|id| !erased_ids.contains(*id))
                    .cloned()
                    .collect();
                if !target_ref.starts_with("src-")
                    && !erased_ids.contains(target_ref)
                    && !claims.iter().any(|c| c == target_ref)
                {
                    direct.push(target_ref.to_string());
                }
                for id in direct {
                    if let Ok(Element::Evidence(row)) = nexus.store.get_element(id.parse()?).await {
                        let source =
                            crate::product::source_from_evidence(&row).map_err(kip_error)?;
                        let mut keys = BTreeSet::new();
                        if self.add_source_keys(&source, &mut keys).await.is_ok() {
                            suppressed.extend(keys);
                        }
                        source_roots.push(id.clone());
                    }
                    let response = self
                        .run_kip_settlement(crate::kip::request_with(
                            "PURGE :id REFERENCE POLICY \"authorized_cascade\" CONFIRM \"PURGE\"",
                            crate::kip::param("id", id.as_str()),
                        ))
                        .await
                        .map_err(kip_error)?;
                    if !crate::kip::succeeded(&response) {
                        let error = crate::kip::error_message(&response);
                        if error.contains("LegalHold") || error.contains("legal hold") {
                            return self
                                .finish_plan(
                                    namespace,
                                    receipt_ref,
                                    "semantic_forgetting",
                                    nexus_targets,
                                    host_surfaces,
                                    source_roots,
                                    ForgetStatus::Blocked,
                                    format!("a legal hold retains {id}"),
                                )
                                .await;
                        }
                        return Err(KipError::internal_error(error));
                    }
                    for change in crate::kip::ok_result(&response)
                        .and_then(|r| r.get("changes"))
                        .and_then(Json::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|change| change["op"] == "purge")
                    {
                        if let Some(purged) = change["id"].as_str() {
                            nexus_targets.push(target(purged, "element", "erased"));
                            erased_ids.insert(purged.to_string());
                        }
                    }
                    if !id.starts_with("E-") && source_roots.is_empty() {
                        partial.get_or_insert_with(|| {
                            format!(
                                "{id} was purged, but the sources it was formed from were not \
                                 enumerated, so erasure of their copies is not verified"
                            )
                        });
                    }
                }
                if !suppressed.is_empty() {
                    self.suppress_sources(&suppressed).await?;
                }
            }
        }

        // Host surfaces: staged bytes, Formation and Recall transcripts,
        // ledgers and caches.
        let mut staged_refs: BTreeSet<String> = suppressed
            .iter()
            .filter_map(|key| key.strip_prefix("memory-source:"))
            .map(str::to_string)
            .collect();
        staged_refs.extend(staged.clone());
        for source_ref in &staged_refs {
            self.erase_staged_source(source_ref).await?;
            host_surfaces.push(("blob".into(), source_ref.clone(), "erased".into()));
        }
        let conversations: BTreeSet<u64> = suppressed
            .iter()
            .filter_map(|key| key.strip_prefix("formation:"))
            .filter_map(|id| id.parse().ok())
            .collect();
        for id in conversations {
            let state = if self.scrub_formation_conversation(id).await? {
                "erased"
            } else {
                "unavailable"
            };
            host_surfaces.push(("replay".into(), format!("formation:{id}"), state.into()));
        }
        if !erased_ids.is_empty() {
            let scrubbed = self.scrub_recall_transcripts(&erased_ids).await?;
            host_surfaces.push((
                "summary".into(),
                format!("recall-transcripts:{scrubbed}"),
                "erased".into(),
            ));
            for id in &erased_ids {
                let _ = self.ledger.forget_entity(id).await;
            }
            let _ = self.miss_cache.clear().await;
            host_surfaces.push(("cache".into(), "usage-ledger".into(), "erased".into()));
        }
        let scope = match input.mode {
            ForgetMode::PayloadOnly => "payload_only",
            ForgetMode::Semantic => "semantic_forgetting",
        };
        let (status, summary) = match partial {
            Some(reason) => (ForgetStatus::Partial, reason),
            None => (
                ForgetStatus::Completed,
                format!(
                    "Erased {} element(s) or payload(s) and {} host copy surface(s).",
                    nexus_targets.len(),
                    host_surfaces.len()
                ),
            ),
        };
        self.finish_plan(
            namespace,
            receipt_ref,
            scope,
            nexus_targets,
            host_surfaces,
            source_roots,
            status,
            summary,
        )
        .await
    }

    /// Validates and retains the plan. `completed` is claimed only when the
    /// Nexus accepts the plan against its actual storage.
    #[allow(clippy::too_many_arguments)]
    async fn finish_plan(
        &self,
        namespace: &str,
        receipt_ref: &str,
        scope: &str,
        targets: Vec<Json>,
        host_surfaces: Vec<(String, String, String)>,
        mut source_roots: Vec<String>,
        mut status: ForgetStatus,
        mut summary: String,
    ) -> Result<(ForgetStatus, String, u64), KipError> {
        source_roots.sort();
        source_roots.dedup();
        let seq = self.memory_seq().await?;
        let external: Vec<String> = self.delivered_bases(&targets).await;
        let mut plan = json!({
            "scope": scope,
            "basis_seq": seq,
            "source_event_refs": source_roots,
            "targets": targets,
            "external_exports": external,
            "status": match status {
                ForgetStatus::Completed => "completed",
                ForgetStatus::Partial => "partial",
                ForgetStatus::Blocked => "blocked",
                ForgetStatus::Pending => "pending",
            },
            "receipts": [receipt_ref],
        });
        if status == ForgetStatus::Completed
            && let Err(error) = self
                .memory
                .nexus()
                .system_session()
                .validate_erasure_plan(DEFAULT_SPACE, &plan)
                .await
        {
            status = ForgetStatus::Partial;
            summary = format!("the erasure could not be verified complete: {error}");
            plan["status"] = json!("partial");
        }
        if host_surfaces.iter().any(|(_, _, state)| state != "erased")
            && status == ForgetStatus::Completed
        {
            status = ForgetStatus::Partial;
            summary = "some host copies could not be verified erased".into();
            plan["status"] = json!("partial");
        }
        let id = &receipt_ref[5..];
        self.memory_interface
            .journal
            .put(
                &format!("plans/{id}"),
                &RetainedPlan {
                    plan_ref: plan_ref(receipt_ref),
                    namespace: namespace.to_string(),
                    receipt_ref: receipt_ref.to_string(),
                    plan,
                    host_surfaces,
                },
                PutMode::Overwrite,
            )
            .await
            .map_err(kip_error)?;
        Ok((status, summary, seq))
    }

    /// Retained recall results that already delivered an erased element: the
    /// copies outside this Brain's guarantee (MI §4).
    async fn delivered_bases(&self, targets: &[Json]) -> Vec<String> {
        use futures::TryStreamExt;
        let erased: BTreeSet<&str> = targets.iter().filter_map(|t| t["ref"].as_str()).collect();
        let mut found = Vec::new();
        let journal = &self.memory_interface.journal;
        let mut keys = journal.keys("recalls/");
        while let Ok(Some(key)) = keys.try_next().await {
            if found.len() >= 128 {
                break;
            }
            let Ok(Some(retained)) = journal.read::<Json>(&key).await else {
                continue;
            };
            let delivered = retained.value["items"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|item| {
                    item[1]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|pin| pin[0].as_str().is_some_and(|id| erased.contains(id)))
                });
            if delivered && let Some(id) = key.strip_prefix("recalls/") {
                found.push(id.to_string());
            }
        }
        found
    }

    async fn held(&self, id: &str) -> bool {
        let Ok(parsed) = id.parse::<ElementId>() else {
            return false;
        };
        let Ok(element) = self.memory.nexus().store.get_element(parsed).await else {
            return false;
        };
        let retention = match &element {
            Element::Concept(r) => &r.retention,
            Element::Proposition(r) => &r.retention,
            Element::Assertion(r) => &r.retention,
            Element::Evidence(r) => &r.retention,
            Element::Activity(r) => &r.retention,
        };
        retention.get("legal_hold") == Some(&Json::Bool(true))
    }

    /// Assertions on a Proposition.
    async fn assertions_on(&self, proposition: &str) -> Result<Vec<String>, KipError> {
        let rows = self
            .execute_kip_readonly(crate::kip::request_with(
                "FIND(?a.id) WHERE { ?p PROPOSITION (id: :id) ?a ASSERTION {proposition: ?p} } LIMIT 128",
                crate::kip::param("id", proposition),
            ))
            .await
            .map_err(kip_error)?;
        Ok(crate::kip::ok_result(&rows)
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                crate::agents::first_row(row.clone())
                    .as_str()
                    .map(str::to_string)
            })
            .collect())
    }

    /// Assertions that cite an Evidence element.
    async fn assertions_citing(&self, evidence: &str) -> Result<Vec<String>, KipError> {
        let referrers = self
            .memory
            .nexus()
            .store
            .referrers(DEFAULT_SPACE, evidence.parse()?)
            .await?;
        Ok(referrers
            .into_iter()
            .filter(|id| id.kind == anda_kip::ElementKind::Assertion)
            .map(|id| id.to_string())
            .collect())
    }

    /// Evidence Formation captured from a staged source's conversations.
    async fn evidence_of_source(&self, source_ref: &str) -> Result<Vec<String>, KipError> {
        use futures::TryStreamExt;
        let journal = &self.memory_interface.journal;
        let mut found = Vec::new();
        let mut keys = journal.keys("receipts/");
        while let Some(key) = keys.try_next().await.map_err(kip_error)? {
            let Some(record) = journal
                .read::<intake::IntakeRecord>(&key)
                .await
                .map_err(kip_error)?
            else {
                continue;
            };
            if record.value.source_ref.as_deref() != Some(source_ref) {
                continue;
            }
            if let Some(conversation) = record.value.conversation {
                for index in 1..=crate::kip::MAX_INGESTED_MESSAGES {
                    if let Some(id) = self
                        .memory
                        .nexus()
                        .store
                        .find_by_client_key(
                            DEFAULT_SPACE,
                            anda_kip::ElementKind::Evidence,
                            &format!("formation:conversation:{conversation}:{index}"),
                        )
                        .await?
                    {
                        found.push(id.to_string());
                    }
                }
            }
            if let Some(result) = &record.value.result {
                for reference in result["memory_refs"].as_array().into_iter().flatten() {
                    if let Some(id) = reference.as_str().filter(|id| id.starts_with("E-")) {
                        found.push(id.to_string());
                    }
                }
            }
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    /// Deletes one claim's closure through the product path, returning the
    /// erased targets and the source keys it suppressed.
    async fn product_delete(
        self: &Arc<Self>,
        receipt_ref: &str,
        claim: &str,
    ) -> Result<(Vec<(String, String)>, BTreeSet<String>), BoxError> {
        let record = self.product_record(claim).await?;
        let operation_id = format!("mi-{}-{}", &receipt_ref[5..21], claim.replace('-', ""));
        let caller = crate::agents::SELF_USER_ID;
        let prepared = self
            .product_prepare(
                caller,
                ChangeInput {
                    operation_id: operation_id.clone(),
                    record_id: claim.to_string(),
                    expected_revision: record.revision,
                    kind: ChangeKind::Delete,
                    new_value: None,
                },
            )
            .await?;
        let confirmed = self
            .product_commit(caller, operation_id, prepared.preview_digest.clone())
            .await?;
        if confirmed.state != "confirmed" {
            return Err(confirmed
                .error
                .unwrap_or_else(|| "memory change did not confirm".into())
                .into());
        }
        Ok((
            confirmed
                .preview
                .targets
                .iter()
                .map(|t| (t.id.clone(), t.kind.clone()))
                .collect(),
            confirmed.preview.excluded_sources.clone(),
        ))
    }

    /// Excludes sources from Formation, so an erased source is never
    /// re-ingested (the forget tombstone).
    pub(crate) async fn suppress_sources(&self, keys: &BTreeSet<String>) -> Result<(), KipError> {
        let _guard = self.product_control.gate.lock().await;
        let mut control = self.product_control.snapshot();
        let before = control.suppressed.len();
        control.suppressed.extend(keys.iter().cloned());
        if control.suppressed.len() == before {
            return Ok(());
        }
        if control.suppressed.len() > 100_000 {
            return Err(KipError::result_limit_exceeded(
                "source suppression capacity exhausted",
            ));
        }
        self.product_control.save(control).await.map_err(kip_error)
    }

    /// Replaces a Formation conversation's copy of its source with a
    /// redaction marker, keeping the processing record.
    async fn scrub_formation_conversation(&self, id: u64) -> Result<bool, KipError> {
        let Ok(conversation) = self.memory.get_conversation(id).await else {
            return Ok(false);
        };
        if conversation.messages.is_empty() {
            return Ok(true);
        }
        let mut changes = std::collections::BTreeMap::new();
        changes.insert("messages".to_string(), anda_db::query::Fv::Array(vec![]));
        self.memory
            .update_conversation(id, changes)
            .await
            .map_err(|e| KipError::internal_error(e.to_string()))?;
        Ok(self
            .memory
            .get_conversation(id)
            .await
            .map(|c| c.messages.is_empty())
            .unwrap_or(false))
    }

    /// Clears Recall transcripts that quoted an erased element. Returns how
    /// many were scrubbed.
    async fn scrub_recall_transcripts(&self, erased: &BTreeSet<String>) -> Result<u64, KipError> {
        let collection = self.recall.conversations_collection.clone();
        let mut scrubbed = 0;
        for id in collection.ids() {
            let Ok(conversation) = self.recall.conversations.get_conversation(id).await else {
                continue;
            };
            let text = serde_json::to_string(&conversation.messages).unwrap_or_default();
            if !erased
                .iter()
                .any(|element| text.contains(&format!("\"{element}\"")))
            {
                continue;
            }
            let mut changes = std::collections::BTreeMap::new();
            changes.insert("messages".to_string(), anda_db::query::Fv::Array(vec![]));
            self.recall
                .conversations
                .update_conversation(id, changes)
                .await
                .map_err(|e| KipError::internal_error(e.to_string()))?;
            scrubbed += 1;
        }
        Ok(scrubbed)
    }
}

fn plan_ref(receipt_ref: &str) -> String {
    format!(
        "plan-{}",
        receipt_ref.strip_prefix("rcpt-").unwrap_or(receipt_ref)
    )
}
