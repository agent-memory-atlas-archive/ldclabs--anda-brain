use super::*;
use crate::kip;

impl UtilityRuntime {
    async fn producer_basis(&self, row: &Json) -> Result<Option<(String, Json)>, BoxError> {
        let id = row["id"].as_str().ok_or("utility target id missing")?;
        let raw = self.nexus.store.get_element(id.parse()?).await?;
        if !anda_cognitive_nexus::schema::contracts::is_derived(&raw) {
            return Ok(None);
        }
        if row["_system"]["dependency_validity"]["action_eligible"] != true {
            return Err("utility cannot revalidate an unverified derived memory".into());
        }
        for tx in [
            row["_system"]["updated_tx"].as_str(),
            row["_system"]["created_tx"].as_str(),
        ]
        .into_iter()
        .flatten()
        {
            let transaction = self
                .nexus
                .store
                .find_transaction(tx)
                .await?
                .ok_or("producer transaction missing")?;
            if transaction.changed_ids.len() > 256 {
                return Err("producer discovery bound exceeded".into());
            }
            for reference in &transaction.changed_ids {
                if !reference.starts_with("X-") {
                    continue;
                }
                let element = self.nexus.store.get_element(reference.parse()?).await?;
                if let anda_cognitive_nexus::store::Element::Activity(activity) = element
                    && activity.status == "completed"
                    && activity.origin["_kip_runtime"]["output_versions"][id]
                        == row["_system"]["version"]
                    && activity.outputs.iter().any(|v| v == id || v["id"] == id)
                    && let Some(basis) = activity
                        .facets
                        .get("kip://profiles/cognitive-memory@2.1.0/DependencyBasis")
                {
                    full_read(&self.nexus.system_session(), reference).await?;
                    return Ok(Some((reference.clone(), basis.clone())));
                }
            }
        }
        // Revalidation can attest an existing version without modifying the
        // Concept, so its transaction need not equal created_tx/updated_tx.
        let query = format!(
            "FIND(?a) WHERE {{?a ACTIVITY {{activity_class:\"dependency_validation\",status:\"completed\"}} FILTER(?a._system.output_versions[{}] == {})}} ORDER BY ?a._system.space_seq DESC LIMIT 1",
            kip::string_literal(id),
            row["_system"]["version"]
        );
        let response = crate::kip::execute_readonly_request(
            &self.nexus.system_session(),
            &kip::request(query),
        )
        .await;
        if let Some(error) = kip::error_of(&response) {
            return Err(format!("utility producer lookup failed: {}", error.message).into());
        }
        if let Some(activity) = kip::ok_result(&response)
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            && let Some(basis) =
                activity["facets"].get("kip://profiles/cognitive-memory@2.1.0/DependencyBasis")
        {
            return Ok(Some((
                activity["id"]
                    .as_str()
                    .ok_or("producer id unavailable")?
                    .into(),
                basis.clone(),
            )));
        }
        Err("exact existing dependency basis unavailable; utility cannot create one".into())
    }
    pub(super) async fn prepare_transaction(
        &self,
        result: &UtilityResult,
        row: &Option<Json>,
        mut evidence: Vec<String>,
        generation: u64,
    ) -> Result<anda_kip::Request, BoxError> {
        let receipt = &result.receipt;
        let producer = if receipt.status == "applied" {
            self.producer_basis(row.as_ref().ok_or("utility target missing")?)
                .await?
        } else {
            None
        };
        let mut pinned_versions = BTreeMap::new();
        if let Some((id, basis)) = &producer {
            evidence.push(id.clone());
            for group in basis["groups"].as_array().into_iter().flatten() {
                for p in group["pins"].as_array().into_iter().flatten() {
                    let id = p["id"]
                        .as_str()
                        .ok_or("dependency source missing")?
                        .to_string();
                    pinned_versions.insert(
                        id.clone(),
                        p["version"].as_u64().ok_or("dependency version missing")?,
                    );
                    evidence.push(id);
                }
            }
        }
        evidence.sort();
        evidence.dedup();
        let mut guards = String::new();
        let mut inputs = BTreeSet::new();
        for id in evidence {
            if inputs.len() >= 512 {
                return Err("utility native evidence budget exceeded".into());
            }
            let view = full_read(&self.nexus.system_session(), &id).await?;
            let version = pinned_versions
                .get(&id)
                .copied()
                .or_else(|| view["_system"]["version"].as_u64())
                .ok_or("utility evidence version missing")?;
            // A native CAS-guarded no-op: preserve the existing immutable
            // observation key, or remove it from an already absent Facet.
            // No row/plane version changes when the input is unchanged. This
            // lets the same native transaction reject a correction racing the
            // utility write, without adding metadata to Evidence/Assertions.
            let held = &view["facets"]["kip://profiles/cognitive-memory@2.1.0/OutcomeRecord"];
            let action = if let Some(key) = held["observation_key"].as_str() {
                format!(
                    "SET FACET \"OutcomeRecord\" {{observation_key:{}}}",
                    kip::string_literal(key)
                )
            } else if held.is_null() {
                "UNSET FACET \"OutcomeRecord\" {observation_key}".into()
            } else {
                return Err("unsupported Outcome guard shape".into());
            };
            guards.push_str(&format!(
                " UPDATE {} {action} EXPECT VERSION {version}",
                kip::string_literal(&id)
            ));
            inputs.insert(id.clone());
        }
        let update = if receipt.status == "applied" {
            let row = row.as_ref().ok_or("utility target unavailable")?;
            let version = row["_system"]["version"]
                .as_u64()
                .ok_or("utility target version missing")?;
            format!(
                "UPDATE {} SET FACET \"MnemonicState\" {{utility:{}}} EXPECT VERSION {version}",
                kip::string_literal(&receipt.target),
                serde_json::to_string(&receipt.new_value.unwrap())?
            )
        } else {
            String::new()
        };
        // Read-only proposals are still immutable artifacts; no Assertion,
        // BELIEF policy, Skill standing or source trust is modified.
        let payload = kip::string_literal(&serde_json::to_string(receipt)?);
        let inputs = inputs
            .iter()
            .map(|id| format!("(\"inputs\",{})", kip::string_literal(id)))
            .collect::<Vec<_>>()
            .join(" ");
        let preserve = if let Some((_, basis)) = &producer {
            let source_edges = pinned_versions
                .keys()
                .map(|id| format!("(\"inputs\",{})", kip::string_literal(id)))
                .collect::<Vec<_>>()
                .join(" ");
            format!(
                r#"CREATE ACTIVITY ?preserve {{SET FIELDS {{activity_class:"utility_metadata_refresh",status:"completed"}} SET FACET "DependencyBasis" {basis} SET STRUCTURAL {{{source_edges} ("outputs",{})}}}}"#,
                kip::string_literal(&receipt.target)
            )
        } else {
            String::new()
        };
        let target_output = if receipt.status == "applied" && producer.is_none() {
            format!("(\"outputs\",{})", kip::string_literal(&receipt.target))
        } else {
            String::new()
        };
        let command = format!(
            r#"MUTATE {{ {guards} {update}
            CREATE EVIDENCE ?receipt {{SET FIELDS {{evidence_class:"artifact",payload:{payload}}}}}
            CREATE ACTIVITY ?calibration {{SET FIELDS {{activity_class:"memory_utility_calibration",status:"completed"}} SET STRUCTURAL {{{inputs} ("outputs",?receipt) {target_output}}}}}
            {preserve}
        }}"#
        );
        let mut request = kip::request(command);
        request.operations[0].idempotency_key = Some(format!(
            "brain-utility:{}:{generation}",
            receipt.calibration_key
        ));
        request.parameters = Some(kip::param(
            "calibration_contract",
            receipt.contract_digest.clone(),
        ));
        Ok(request)
    }
    async fn recovered(&self, pending: &Pending) -> Result<Option<String>, BoxError> {
        let key = pending.request.operations[0]
            .idempotency_key
            .as_deref()
            .ok_or("utility commit key missing")?;
        let response = anda_kip::execute_request(
            &self.nexus.system_session(),
            &kip::request(format!(
                "DESCRIBE TRANSACTION BY IDEMPOTENCY KEY {}",
                kip::string_literal(key)
            )),
        )
        .await;
        if let Some(error) = kip::error_of(&response) {
            if error.parsed_code() == Some(anda_kip::KipErrorCode::TransactionUnknown) {
                return Ok(None);
            }
            return Err(kip::error_message(&response).into());
        }
        let tx = kip::ok_result(&response)
            .and_then(|r| r["tx_id"].as_str())
            .ok_or("utility transaction lookup missing")?;
        let tx = self
            .nexus
            .store
            .find_transaction(tx)
            .await?
            .ok_or("utility transaction unavailable")?;
        if tx.status != "committed" || tx.origin["principal_id"] != "kip:principal:system" {
            return Err("utility transaction not committed by its controller".into());
        }
        // Resolve by exact payload among the committed transaction's outputs.
        for id in &tx.changed_ids {
            if id.starts_with("E-") {
                let row = full_read(&self.nexus.system_session(), id).await?;
                if row["payload"]["inline"] == serde_json::to_string(&pending.result.receipt)? {
                    return Ok(Some(id.clone()));
                }
            }
        }
        Err("committed utility receipt missing".into())
    }
    pub(super) async fn commit(&self, target: &mut Target) -> Result<(), BoxError> {
        let pending = target
            .pending
            .as_ref()
            .ok_or("utility intent missing")?
            .clone();
        self.nexus.recover().await?;
        let reference = if let Some(reference) = self.recovered(&pending).await? {
            reference
        } else {
            for unit in &pending.samples {
                let key = self.key("units", unit)?;
                if let Some(held) = self.directory.read::<UnitClaim>(&key).await? {
                    if held.value.target != target.id
                        || held.value.calibration_key != pending.result.receipt.calibration_key
                    {
                        return Err(
                            "independent sample already reserved for another attribution".into(),
                        );
                    }
                } else {
                    self.directory
                        .create(
                            &key,
                            &UnitClaim {
                                target: target.id.clone(),
                                calibration_key: pending.result.receipt.calibration_key.clone(),
                                receipt: None,
                            },
                        )
                        .await?;
                }
            }
            if pending.result.receipt.status == "applied" {
                for (id, digest) in &pending.evidence {
                    let current = full_read(&self.nexus.system_session(), id).await;
                    if !current.is_ok_and(|row| {
                        semantic_digest(&row).ok().as_ref() == Some(digest) && trace::usable(&row)
                    }) {
                        target.pending = None;
                        target.generation += 1;
                        self.save(&self.key("targets", &target.id)?, target).await?;
                        return Err(
                            "utility evidence changed before commit; recompute on next pass".into(),
                        );
                    }
                }
            }
            let response =
                anda_kip::execute_request(&self.nexus.system_session(), &pending.request).await;
            if let Some(error) = kip::error_of(&response) {
                if error.parsed_code() == Some(anda_kip::KipErrorCode::VersionConflict)
                    && self.recovered(&pending).await?.is_none()
                {
                    target.pending = None;
                    target.generation += 1;
                    self.save(&self.key("targets", &target.id)?, target).await?;
                }
                return Err(format!("utility native commit unresolved: {}", error.message).into());
            }
            kip::ok_result(&response)
                .and_then(|r| r["handles"]["receipt"].as_str())
                .ok_or("utility receipt acknowledgement missing")?
                .to_string()
        };
        let mut result = pending.result;
        result.evidence_ref = Some(reference);
        for unit in &pending.samples {
            self.save(
                &self.key("units", unit)?,
                &UnitClaim {
                    target: target.id.clone(),
                    calibration_key: result.receipt.calibration_key.clone(),
                    receipt: result.evidence_ref.clone(),
                },
            )
            .await?;
        }
        if result.receipt.status == "applied" {
            target.applied = Some(result.clone());
            target.rank_pin = result.receipt.target_pin.clone();
            target.rank_evidence = pending.evidence;
        }
        if result.receipt.status == "applied" {
            for id in &result.receipt.selected_outcomes {
                target.outcomes.remove(id);
            }
        }
        // Exclusions are retained in immutable receipts, not forever in the hot
        // sample set. Insufficient eligible groups remain queued for more data.
        for id in result.receipt.excluded.keys() {
            target.outcomes.remove(id);
        }
        target.last = Some(result.clone());
        target.pending = None;
        self.directory
            .put(
                &self.key("results", &result.receipt.calibration_key)?,
                &result,
                PutMode::Create,
            )
            .await?;
        self.save(&self.key("targets", &target.id)?, target).await
    }
}
