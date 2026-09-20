use super::runtime::*;
use super::*;
use anda_cognitive_nexus::{
    nexus::DEFAULT_SPACE,
    trust::{TrustCalibrationProposal, TrustConfiguration},
};
use std::{collections::BTreeSet, sync::Arc};

impl TrustRuntime {
    pub(super) async fn trust_control(&self) -> Result<(u64, TrustConfiguration), BoxError> {
        let row = self
            .proposer()?
            .read_control(DEFAULT_SPACE, "trust", None)
            .await?
            .ok_or("native trust configuration unavailable")?;
        // The control also contains native calibration provenance. Preserve all
        // actual weighting fields without interpreting that metadata as a rule.
        Ok((
            row.version,
            TrustConfiguration {
                weights: serde_json::from_value(row.value["weights"].clone())?,
                default_weight: row.value["default_weight"]
                    .as_f64()
                    .ok_or("trust default weight missing")?,
                rules: serde_json::from_value(
                    row.value.get("rules").cloned().unwrap_or_else(|| json!([])),
                )?,
            },
        ))
    }
    pub async fn propose(self: &Arc<Self>, actor: String) -> Result<TrustProposal, BoxError> {
        let this = self.clone();
        self.tasks
            .run(async move {
                let _g = this.gate.lock().await;
                this.propose_inner(&actor, true).await
            })
            .await
    }
    pub(super) async fn propose_inner(
        &self,
        actor: &str,
        explicit: bool,
    ) -> Result<TrustProposal, BoxError> {
        let cfg = self.cfg()?;
        self.catalog().await?;
        let key = self.target_key(actor)?;
        let mut target = self
            .journal
            .read::<Target>(&key)
            .await?
            .ok_or("no registered factual verification for this source")?
            .value;
        if target.contract != cfg.contract_digest()? {
            return Err("trust target contract changed".into());
        }
        if let Some(latest) = &target.latest {
            let job = self.job(latest).await?;
            if job.reviewed.is_some() && job.receipt.is_none() && !job.stale {
                return self.proposal(latest).await;
            }
            if job.stale && !explicit {
                return self.proposal(latest).await;
            }
            if job.stale && explicit {
                target.epoch = target
                    .epoch
                    .checked_add(1)
                    .filter(|v| *v <= anda_kip::MAX_SAFE_INTEGER)
                    .ok_or("trust review generation exhausted")?;
                self.save(&key, &target).await?;
            }
        }
        let mut excluded = BTreeMap::new();
        let mut unresolved = false;
        let mut candidates = Vec::new();
        let mut sources = BTreeSet::from([actor.to_string(), cfg.context_ref.clone()]);
        for source in target.sources.values_mut() {
            let Some(id) = self.resolve_source(source).await? else {
                unresolved = true;
                excluded.insert(
                    source.client_key.clone(),
                    "verification_not_committed".into(),
                );
                continue;
            };
            source.reference = Some(id.clone());
            sources.insert(id.clone());
            match self.sample(&id).await {
                Ok(sample) if sample.actor == actor => {
                    sources.extend(sample.sources.clone());
                    if let Some(reason) = sample.reason.clone() {
                        excluded.insert(id, reason);
                    } else {
                        candidates.push(sample);
                    }
                }
                Err(error) => {
                    if error.downcast_ref::<Ineligible>().is_none() {
                        unresolved = true;
                    }
                    excluded.insert(id, "verification_ineligible_or_unavailable".into());
                }
                _ => {
                    excluded.insert(id, "verification_actor_mismatch".into());
                }
            }
        }
        candidates.sort_by(|a, b| (a.sequence, &a.reference).cmp(&(b.sequence, &b.reference)));
        let mut seen = BTreeSet::new();
        let mut selected = Vec::new();
        let needed = cfg.parameters.as_ref().map_or(32, |p| p.minimum_samples);
        for sample in candidates {
            if !seen.insert(sample.unit.clone()) || !seen.insert(sample.claim_group.clone()) {
                excluded.insert(sample.reference, "duplicate_evidence_root".into());
                continue;
            }
            let mut consumed = false;
            for unit in [&sample.unit, &sample.claim_group] {
                consumed |= self
                    .journal
                    .read::<RootClaim>(&self.key("units", unit)?)
                    .await?
                    .is_some_and(|r| !r.value.abandoned);
            }
            if consumed {
                excluded.insert(sample.reference, "root_already_consumed_or_reserved".into());
                continue;
            }
            if selected.len() < needed {
                selected.push(sample);
            } else {
                excluded.insert(sample.reference, "outside_fixed_first_n_group".into());
            }
        }
        let (version, mut current) = self.trust_control().await?;
        let previous = current
            .rules
            .iter()
            .find(|r| {
                r.actor_ref == actor
                    && r.predicate_ref.as_ref() == Some(&cfg.predicate_ref)
                    && r.context_ref.as_ref() == Some(&cfg.context_ref)
            })
            .cloned();
        let old_weight = current.weight(
            actor,
            &cfg.predicate_ref,
            std::slice::from_ref(&cfg.context_ref),
        )?;
        let evidence: Vec<_> = selected.iter().map(|s| s.reference.clone()).collect();
        let roots: Vec<_> = selected
            .iter()
            .flat_map(|s| [s.unit.clone(), s.claim_group.clone()])
            .collect();
        let n = selected.len();
        let mut uncertainty = json!({"method":"binary-fact-accuracy-v1","selection":"fixed-first-n-by-native-creation-seq","n":n,
            "root_independence":"registered-instrument-assumption; replicas share a root","parameters":cfg.parameters,
            "scope":{"actor":actor,"predicate":cfg.predicate_ref,"context":cfg.context_ref,"task_family":cfg.task_family,"environment":cfg.environment_digest}});
        let (new_weight, reason) = match &cfg.parameters {
            _ if unresolved => (
                None,
                Some("unresolved_verification_intake_or_material".into()),
            ),
            None => (None, Some("trust_mapping_parameters_missing".into())),
            Some(p) if n < p.minimum_samples => {
                (None, Some("insufficient_independent_verified_facts".into()))
            }
            Some(p) => {
                let accuracy =
                    selected.iter().filter(|s| s.correct == Some(true)).count() as f64 / n as f64;
                let radius = ((2.0 / p.alpha).ln() / (2.0 * n as f64)).sqrt();
                uncertainty["accuracy"] = json!(accuracy);
                uncertainty["lower_bound"] = json!((accuracy - radius).max(0.0));
                uncertainty["upper_bound"] = json!((accuracy + radius).min(1.0));
                uncertainty["confidence"] = json!(1.0 - p.alpha);
                let next = (old_weight
                    + (p.gain * (accuracy - old_weight)).clamp(-p.step_cap, p.step_cap))
                .clamp(0.0, 1.0);
                (
                    Some(next),
                    (!cfg.calibrated()).then(|| {
                        "proposal_requires_method_calibration_and_governance_review".into()
                    }),
                )
            }
        };
        let proposed = new_weight.map(|weight| ContextualTrustRule {
            id: previous.as_ref().map(|r| r.id.clone()).unwrap_or_else(|| {
                format!(
                    "brain-fact-{}",
                    &content_digest(&json!([actor, &cfg.predicate_ref, &cfg.context_ref]))
                        .expect("canonical scope")[7..]
                )
            }),
            actor_ref: actor.into(),
            predicate_ref: Some(cfg.predicate_ref.clone()),
            context_ref: Some(cfg.context_ref.clone()),
            weight,
        });
        let basis_digest = content_digest(&json!(current))?;
        uncertainty["trust_configuration_digest"] = json!(basis_digest);
        let id = content_digest(
            &json!({"scope":self.scope,"contract":cfg.contract_digest()?,"actor":actor,"version":version,"configuration":basis_digest,"epoch":target.epoch,"evidence":evidence,"excluded":excluded}),
        )?;
        if self
            .journal
            .read::<ProposalJob>(&self.key("proposals", &id)?)
            .await?
            .is_some()
        {
            // Publication may have committed before the review-inventory ACK.
            // Repair the pointer from the immutable result instead of hiding
            // a completed proposal forever after a dropped checkpoint.
            let result = self.proposal(&id).await?;
            target.latest = Some(id);
            self.save(&key, &target).await?;
            return Ok(result);
        }
        let sources: Vec<_> = sources.into_iter().collect();
        let method=self.proposer()?.put_artifact(DEFAULT_SPACE,json!({"format":FORMAT,"method":"binary-fact-accuracy-v1","contract":cfg,"contract_digest":cfg.contract_digest()?,"scope":self.scope,"sources":sources,"expected_version":version}),sources.clone()).await?;
        let native = if let Some(rule) = &proposed {
            if let Some(old) = current.rules.iter_mut().find(|r| r.id == rule.id) {
                if previous.as_ref().is_none_or(|p| p.id != old.id) {
                    return Err("trust rule identity collision".into());
                }
                *old = rule.clone();
            } else {
                current.rules.push(rule.clone());
            }
            current.validate()?;
            let native = TrustCalibrationProposal {
                format: "nexus:trust-calibration-v1".into(),
                space_id: DEFAULT_SPACE.into(),
                expected_version: version,
                configuration: current,
                method: method.clone(),
                evidence_refs: evidence.clone(),
                uncertainty: uncertainty.clone(),
            };
            Some(
                self.proposer()?
                    .put_artifact(DEFAULT_SPACE, json!(native), sources.clone())
                    .await?,
            )
        } else {
            None
        };
        let proposal = TrustProposal {
            format: FORMAT.into(),
            id: id.clone(),
            scope: self.scope.clone(),
            contract_digest: cfg.contract_digest()?,
            actor_ref: actor.into(),
            predicate_ref: cfg.predicate_ref.clone(),
            context_ref: cfg.context_ref.clone(),
            expected_version: version,
            previous_rule: previous,
            proposed_rule: proposed,
            old_weight,
            new_weight,
            independent_samples: n,
            evidence_refs: evidence,
            roots,
            excluded,
            uncertainty,
            method,
            native_proposal: native,
            reason,
            restores: None,
        };
        let result = self.publish(proposal, sources).await?;
        target.sources.retain(|_, s| {
            s.reference.as_ref().is_none_or(|id| {
                result.excluded.get(id).is_none_or(|reason| {
                    reason == "outside_fixed_first_n_group"
                        || reason == "verification_not_committed"
                        || reason == "verification_ineligible_or_unavailable"
                })
            })
        });
        target.latest = Some(id);
        self.save(&key, &target).await?;
        Ok(result)
    }
    /// Bounded review inventory. Details still require governed artifact reads.
    pub async fn proposals(
        &self,
        after: usize,
        limit: usize,
    ) -> Result<(Vec<TrustProposal>, Option<usize>), BoxError> {
        if !(1..=32).contains(&limit) {
            return Err("trust review page limit must be 1..=32".into());
        }
        let catalog = self
            .journal
            .read::<Catalog>("catalog")
            .await?
            .map(|v| v.value)
            .unwrap_or_default();
        let end = after.saturating_add(limit).min(catalog.targets.len());
        let mut items = vec![];
        for key in catalog.targets.get(after..end).unwrap_or_default() {
            if let Some(target) = self.journal.read::<Target>(key).await?
                && let Some(id) = target.value.latest
            {
                items.push(self.proposal(&id).await?);
            }
        }
        Ok((items, (end < catalog.targets.len()).then_some(end)))
    }
}
