//! Bounded context, host decisions and deterministic clarification workers.
use super::*;
use super::{
    context::Capture,
    journal::{Job, Reply, key},
};
use anda_cognitive_nexus::{attention::WakeState, nexus::DEFAULT_SPACE};

impl ActionRuntime {
    pub(super) async fn drive(&self, supplied: &WakeRecord, job: &mut Job) -> Result<(), BoxError> {
        let session = self.session(&supplied.scope).await?;
        let mut wake = session.read_wake(DEFAULT_SPACE, &supplied.wake_ref).await?;
        if matches!(wake.state, WakeState::Cancelled { .. }) {
            job.status.state = "cancelled".into();
            return Ok(());
        }
        if matches!(wake.state, WakeState::Completed { .. }) {
            if job.prepared.is_some() {
                self.commit(&session, &wake, job).await?;
            }
            return Ok(());
        }
        let parent = if let Some(parent) = &wake.parent_ref {
            Some(self.confirm_parent(&session, parent, &wake.scope).await?)
        } else {
            None
        };
        if let Some(parent) = &parent
            && job.status.operator_retries == 0
        {
            job.rounds =
                parent.rounds + u32::from(wake.continuation_key.as_deref() == Some("retry"));
        }
        if wake.continuation_key.as_deref() == Some("dispatch") {
            return self
                .dispatch(
                    &session,
                    &mut wake,
                    job,
                    parent.as_ref().ok_or("dispatch parent missing")?,
                )
                .await;
        }
        if job.status.state == "blocked" {
            return Ok(());
        }
        let mut reply = None;
        let mut timed_out = false;
        if wake.continuation_key.as_deref() == Some("answer") {
            let parent = parent.as_ref().ok_or("answer parent missing")?;
            let prepared = parent
                .prepared
                .as_ref()
                .ok_or("answer preparation missing")?;
            let saved = self
                .directory
                .read::<Reply>(&key(&wake.scope, "replies", &parent.wake_ref)?)
                .await?;
            if let Some(saved) = saved {
                let saved = saved.value;
                if saved.scope != wake.scope
                    || saved.gate_wake_ref != parent.wake_ref
                    || Some(&saved.principal) != prepared.recipient.as_ref()
                    || prepared
                        .reply_deadline_ms
                        .is_none_or(|t| saved.received_ms >= t)
                {
                    return Err("reply correlation mismatch".into());
                }
                reply = Some(saved.response);
            } else if prepared
                .reply_deadline_ms
                .is_some_and(|t| t > anda_engine::unix_ms())
            {
                job.status.state = "awaiting_reply".into();
                job.status.next_run_ms =
                    Some(anda_engine::unix_ms() + self.bindings.limits.retry_ms.min(5_000));
                return Ok(());
            } else {
                timed_out = true;
                job.rounds = self.bindings.limits.max_retries;
            }
        }
        self.acquire(&session, &mut wake, job).await?;
        if wake.continuation_key.as_deref() == Some("manual") && job.status.operator_retries == 0 {
            job.status.reason = Some("retry_limit_requires_operator".into());
            return self
                .block(&session, &wake, job, "binding_unavailable")
                .await;
        }
        if wake.continuation_key.as_deref() == Some("clarification") && job.prepared.is_none() {
            let parent = parent.as_ref().ok_or("clarification parent missing")?;
            let p = parent
                .prepared
                .as_ref()
                .ok_or("clarification preparation missing")?;
            if p.decision != "ask" {
                return Err("clarification must follow an ask decision".into());
            }
            let parent_ref = parent
                .status
                .decision_ref
                .clone()
                .ok_or("ask decision unavailable")?;
            let mut context = p.capture.request.clone();
            context.recall_receipt = None;
            context.required_refs.push(parent_ref.clone());
            context.premises.clear();
            context.applied_revisions.clear();
            context.task_family = "brain.deliver_clarification".into();
            context.deduplication_key = None;
            let capture = self
                .bounded(Capture::read(
                    &session,
                    &wake,
                    context,
                    &self.bindings.limits,
                ))
                .await?;
            let answered = self
                .answered(
                    &wake.scope,
                    &parent.wake_ref,
                    p.recipient
                        .as_deref()
                        .ok_or("clarification recipient missing")?,
                    p.reply_deadline_ms
                        .ok_or("clarification deadline missing")?,
                )
                .await?;
            let proposal = if answered {
                Proposal::Silence {
                    rationale: "clarification_already_answered".into(),
                    used_refs: vec![parent_ref.clone()],
                }
            } else if p
                .reply_deadline_ms
                .is_none_or(|t| t <= anda_engine::unix_ms())
            {
                Proposal::Defer {
                    reason: "clarification_expired_before_delivery".into(),
                }
            } else if let Some(reason) = &capture.insufficient {
                Proposal::Defer {
                    reason: reason.clone(),
                }
            } else {
                Proposal::Act {
                    rationale:
                        "Deliver the parent question only; this grants no business permission"
                            .into(),
                    used_refs: vec![parent_ref],
                    payload: p
                        .clarification_payload
                        .clone()
                        .ok_or("question payload unavailable")?,
                }
            };
            let mut prepared = self
                .prepare(&session, &wake, job, capture.clone(), proposal)
                .await?;
            if let Some(request) = &prepared.request
                && let Err(e) = self.authorize(request).await
            {
                job.rounds = self.bindings.limits.max_retries;
                prepared = self
                    .prepare(
                        &session,
                        &wake,
                        job,
                        capture,
                        Proposal::Defer {
                            reason: format!("clarification_authorization_unavailable: {e}")
                                .chars()
                                .take(512)
                                .collect(),
                        },
                    )
                    .await?;
            }
            if prepared.decision == "defer" {
                for child in &mut prepared.continuations {
                    child.key = "manual".into();
                }
            }
            job.prepared = Some(prepared);
            self.save(job).await?;
        }
        if let Some(prepared) = job.prepared.as_mut() {
            if prepared.expected != wake.version || prepared.fence != wake.fence {
                prepared
                    .capture
                    .revalidate(
                        &session,
                        &wake,
                        &self.bindings.limits,
                        prepared.decision == "ask",
                    )
                    .await?;
                prepared.expected = wake.version;
                prepared.fence = wake.fence;
                self.save(job).await?;
            }
        } else {
            let request = self.bounded(self.bindings.policy.context(&wake)).await?;
            let capture = self
                .bounded(Capture::read(
                    &session,
                    &wake,
                    request,
                    &self.bindings.limits,
                ))
                .await?;
            let input = GateInput {
                wake: wake.clone(),
                packet: capture.packet.clone(),
                reply,
                proposal_token_limit: self.bindings.limits.proposal_tokens,
            };
            let input_fits = crate::recall_budget::count(&serde_json::to_string(&input)?)?
                <= self.bindings.limits.recall.context_tokens as usize;
            let mut proposal = if !input_fits {
                Proposal::Defer {
                    reason: "gate_input_budget_exhausted".into(),
                }
            } else if timed_out {
                Proposal::Defer {
                    reason: "clarification_timeout_no_consent".into(),
                }
            } else if let Some(reason) = capture
                .insufficient
                .as_ref()
                .filter(|r| r.as_str() != "premise_unknown_or_not_accepted")
            {
                Proposal::Defer {
                    reason: reason.clone(),
                }
            } else {
                self.bounded(self.bindings.policy.suggest(&input)).await?
            };
            if crate::recall_budget::count(&serde_json::to_string(&proposal)?)?
                > self.bindings.limits.proposal_tokens
            {
                proposal = Proposal::Defer {
                    reason: "proposal_budget_exhausted".into(),
                };
            }
            let reason = match &proposal {
                Proposal::Act { rationale, .. }
                | Proposal::Ask { rationale, .. }
                | Proposal::Silence { rationale, .. } => rationale,
                Proposal::Defer { reason } => reason,
            };
            if reason.trim().is_empty() {
                proposal = Proposal::Defer {
                    reason: "proposal_rationale_missing".into(),
                };
            }
            let used = match &proposal {
                Proposal::Act { used_refs, .. }
                | Proposal::Ask { used_refs, .. }
                | Proposal::Silence { used_refs, .. } => used_refs.as_slice(),
                _ => &[],
            };
            if used.len() > 64
                || used
                    .iter()
                    .any(|r| !input.packet.items.iter().any(|i| &i.id == r))
            {
                proposal = Proposal::Defer {
                    reason: "used_reference_not_delivered".into(),
                };
            }
            match &proposal {
                Proposal::Act { used_refs, .. }
                    if capture
                        .request
                        .applied_revisions
                        .iter()
                        .any(|r| !used_refs.contains(r)) =>
                {
                    proposal = Proposal::Defer {
                        reason: "applied_revision_not_used".into(),
                    }
                }
                Proposal::Act { .. } if capture.insufficient.is_some() => {
                    proposal = Proposal::Defer {
                        reason: capture.insufficient.clone().unwrap(),
                    }
                }
                Proposal::Act { .. } if self.bindings.business.is_none() => {
                    proposal = Proposal::Defer {
                        reason: "business_binding_unavailable".into(),
                    }
                }
                Proposal::Ask { question, .. }
                    if self.bindings.clarification.is_none() || question.trim().is_empty() =>
                {
                    proposal = Proposal::Defer {
                        reason: "clarification_binding_or_question_unavailable".into(),
                    }
                }
                Proposal::Silence { rationale, .. }
                    if !self
                        .bounded(self.bindings.policy.allow_silence(&input, rationale))
                        .await? =>
                {
                    proposal = Proposal::Defer {
                        reason: "silence_not_authorized_by_policy".into(),
                    }
                }
                _ => {}
            }
            let mut prepared = self
                .prepare(&session, &wake, job, capture.clone(), proposal)
                .await?;
            if let Some(request) = &prepared.request {
                let authorized = self.authorize(request).await;
                if let Err(e) = authorized {
                    prepared = self
                        .prepare(
                            &session,
                            &wake,
                            job,
                            capture.clone(),
                            Proposal::Defer {
                                reason: format!("authorization_unavailable: {e}")
                                    .chars()
                                    .take(512)
                                    .collect(),
                            },
                        )
                        .await?;
                } else if request.kind == ActionKind::Business
                    && let Some(reason) = self.deduplicate(&session, request).await?
                {
                    let proposal = if reason.starts_with("duplicate_committed_operation:") {
                        Proposal::Silence {
                            rationale: reason,
                            used_refs: vec![],
                        }
                    } else {
                        Proposal::Defer { reason }
                    };
                    prepared = self
                        .prepare(&session, &wake, job, capture, proposal)
                        .await?;
                }
            }
            job.prepared = Some(prepared);
            self.save(job).await?;
        }
        self.commit(&session, &wake, job).await
    }
}
