use super::*;
use anda_cognitive_nexus::{
    attention::{WakeContinuation, WakeState},
    content_digest,
    nexus::{DEFAULT_SPACE, Session},
};
use context::Capture;
use journal::{Job, PreparedGate};
use serde_json::json;

fn decision_command(
    handle: &str,
    kind: &str,
    rationale: &str,
    capture: &Capture,
    used: &[String],
    revisions: &[String],
    parent: bool,
) -> String {
    let rationale = if let Some(receipt) = &capture.receipt {
        json!({"rationale":rationale,"recall_receipt":receipt}).to_string()
    } else {
        rationale.into()
    };
    let decision = json!({"decision":kind,"rationale":rationale,"retrieved_refs":capture.retrieved,
        "used_refs":used,"applied_revisions":revisions,"basis":capture.basis});
    let mut groups = vec![json!({"role":"context","pins":capture.pins})];
    let required: Vec<_> = capture
        .pins
        .iter()
        .filter(|p| {
            p["id"].as_str().is_some_and(|id| {
                capture
                    .request
                    .premises
                    .iter()
                    .chain(&capture.request.required_refs)
                    .chain(&capture.request.applied_revisions)
                    .any(|r| r == id)
            })
        })
        .cloned()
        .collect();
    if !required.is_empty() && kind == "act" && !parent {
        groups.push(json!({"role":"all_of","pins":required}));
    }
    let dependency = json!({"basis_seq":capture.basis["snapshot_seq"],"policy_basis":capture.basis,"groups":groups});
    let mut inputs = capture
        .retrieved
        .iter()
        .map(|r| format!("(\"inputs\",{})", crate::kip::string_literal(r)))
        .collect::<Vec<_>>()
        .join(" ");
    if parent {
        inputs.push_str(" (\"inputs\",?decision)");
    }
    format!(
        r#"CREATE ACTIVITY ?{handle} {{SET FIELDS {{activity_class:"action_gate",status:"completed"}} SET FACET "DecisionRecord" {decision} SET FACET "DependencyBasis" {dependency} SET STRUCTURAL {{{inputs}}}}}"#
    )
}

impl ActionRuntime {
    pub(super) async fn prepare(
        &self,
        session: &Session,
        wake: &WakeRecord,
        job: &Job,
        mut capture: Capture,
        proposal: Proposal,
    ) -> Result<PreparedGate, BoxError> {
        let receipts = self.receipts.read().clone();
        if let Some(receipts) = receipts {
            if let Some(reference) = &capture.request.recall_receipt {
                let receipt = receipts.read(reference).await?;
                let used = match &proposal {
                    Proposal::Act { used_refs, .. }
                    | Proposal::Ask { used_refs, .. }
                    | Proposal::Silence { used_refs, .. } => used_refs.as_slice(),
                    _ => &[],
                };
                if !receipt.complete_inventory || receipt.delivery != "bounded_packet" {
                    return Err("action requires a verified bounded delivery receipt".into());
                }
                for id in used.iter().chain(&capture.request.applied_revisions) {
                    let row =
                        context::element(session, id, None, self.bindings.limits.callbacks_ms)
                            .await?;
                    if !receipt.pins.iter().any(|p| {
                        p.id == *id
                            && crate::recall_receipt::semantic_digest(&row).ok().as_ref()
                                == Some(&p.content_digest)
                    }) {
                        return Err("used memory was not delivered or its content changed".into());
                    }
                }
                capture.receipt = Some(reference.clone());
            } else {
                capture.receipt = Some(
                    receipts
                        .issue_packet(
                            format!("gate:{}:{}", wake.wake_ref, job.rounds),
                            serde_json::to_string(&capture.packet)?,
                            self.bindings.limits.recall.clone(),
                        )
                        .await?,
                );
            }
        }
        let mut request = None;
        let mut recipient = None;
        let mut reply_deadline_ms = None;
        let mut clarification_payload = None;
        let mut continuations = vec![];
        let (kind, rationale, used, payload) = match proposal {
            Proposal::Act {
                rationale,
                used_refs,
                payload,
            } => (
                "act",
                rationale,
                used_refs,
                Some((
                    if wake.continuation_key.as_deref() == Some("clarification") {
                        ActionKind::DeliverClarification
                    } else {
                        ActionKind::Business
                    },
                    payload,
                )),
            ),
            Proposal::Ask {
                rationale,
                used_refs,
                question,
            } => {
                let channel = self
                    .bindings
                    .clarification
                    .as_ref()
                    .ok_or("clarification binding missing")?;
                recipient = Some(channel.recipient_principal.clone());
                let due = anda_engine::unix_ms() + channel.reply_timeout_ms;
                reply_deadline_ms = Some(due);
                continuations.push(WakeContinuation {
                    key: "answer".into(),
                    not_before_ms: anda_engine::unix_ms(),
                });
                clarification_payload = Some(
                    json!({"question":question,"recipient":channel.recipient_principal,"correlation":wake.wake_ref,"reply_deadline_ms":due}),
                );
                continuations.push(WakeContinuation {
                    key: "clarification".into(),
                    not_before_ms: anda_engine::unix_ms(),
                });
                ("ask", rationale, used_refs, None)
            }
            Proposal::Defer { reason } => {
                continuations.push(WakeContinuation {
                    key: if job.rounds >= self.bindings.limits.max_retries {
                        "manual"
                    } else {
                        "retry"
                    }
                    .into(),
                    not_before_ms: anda_engine::unix_ms() + self.bindings.limits.retry_ms,
                });
                ("defer", reason, vec![], None)
            }
            Proposal::Silence {
                rationale,
                used_refs,
            } => ("silence", rationale, used_refs, None),
        };
        let revisions = if kind == "act" {
            capture.request.applied_revisions.clone()
        } else {
            vec![]
        };
        let mut command = decision_command(
            "decision", kind, &rationale, &capture, &used, &revisions, false,
        );
        if let Some((action_kind, payload)) = payload {
            let attempt_id = format!(
                "brain-action:{}",
                &content_digest(
                    &json!({"scope":wake.scope,"wake":wake.wake_ref,"kind":action_kind})
                )?[7..]
            );
            let action = ActionRequest {
                scope: wake.scope.clone(),
                pins: wake.pins.clone(),
                gate_wake_ref: wake.wake_ref.clone(),
                kind: action_kind.clone(),
                attempt_id: attempt_id.clone(),
                payload,
                context: capture.request.clone(),
            };
            // Selection policy stores the exact request/budgets before the
            // native output transaction. A crash may leave an unused artifact,
            // but cannot leave an executable Attempt without its policy.
            let selection = session
                .put_artifact(
                    DEFAULT_SPACE,
                    json!({"configuration":self.bindings.manifest(),"request":action}),
                    capture.retrieved.clone(),
                )
                .await?;
            let handle = "decision";
            let context = json!({"task_family":if kind=="ask" { "brain.deliver_clarification" } else { &capture.request.task_family },
                "gate_wake_ref":wake.wake_ref,"binding":self.bindings.binding_pin,"request_digest":content_digest(&json!(action))?});
            let mut attempt = json!({"attempt_id":attempt_id,"applied_revisions":revisions,"trial_ref":null,"context":context,
                "environment_digest":capture.request.environment_digest,"tool_versions":capture.request.tool_versions,
                "selection_policy":selection,"preconditions_satisfied":"yes","started_at":anda_cognitive_nexus::time::now()}).to_string();
            attempt.pop();
            attempt.push_str(&format!(",\"decision_ref\":?{handle}}}"));
            command.push_str(&format!(r#" CREATE ACTIVITY ?attempt {{SET FIELDS {{activity_class:"action_attempt",status:"completed"}} SET FACET "AttemptRecord" {attempt} SET STRUCTURAL {{("inputs",?{handle})}}}}"#));
            request = Some(action);
            continuations.push(WakeContinuation {
                key: "dispatch".into(),
                not_before_ms: anda_engine::unix_ms(),
            });
        }
        command = format!("MUTATE {{ {command} }}");
        if command.len() > 65_536 {
            return Err("gate commit exceeds native byte budget".into());
        }
        Ok(PreparedGate {
            command,
            parameters: Default::default(),
            continuations,
            expected: wake.version,
            fence: wake.fence,
            decision: kind.into(),
            capture,
            request,
            recipient,
            reply_deadline_ms,
            clarification_payload,
        })
    }

    pub(super) async fn commit(
        &self,
        session: &Session,
        wake: &WakeRecord,
        job: &mut Job,
    ) -> Result<(), BoxError> {
        let p = job.prepared.as_ref().ok_or("gate has no prepared commit")?;
        let result = session
            .finish_wake(
                DEFAULT_SPACE,
                &wake.wake_ref,
                p.expected,
                p.fence,
                &p.command,
                p.parameters.clone(),
                p.continuations.clone(),
            )
            .await?;
        job.outputs = result["outputs"]
            .as_array()
            .ok_or("native gate receipt lacks outputs")?
            .iter()
            .map(|v| v.as_str().map(String::from).ok_or("invalid gate output"))
            .collect::<Result<_, _>>()?;
        self.read_outputs(session, job).await?;
        if let Some(request) = job.prepared.as_ref().and_then(|p| p.request.as_ref()) {
            job.status.dispatch_ref =
                Some(dispatch_reference(&request.scope, &request.attempt_id)?);
        }
        job.status.state = "committed".into();
        job.status.decision = Some(p_decision(job)?);
        job.status.next_run_ms = None;
        job.status.reason = None;
        Ok(())
    }

    pub(super) async fn read_outputs(
        &self,
        session: &Session,
        job: &mut Job,
    ) -> Result<(), BoxError> {
        let kind = p_decision(job)?;
        for id in &job.outputs {
            if !id.starts_with("X-") {
                continue;
            }
            let row =
                context::element(session, id, None, self.bindings.limits.callbacks_ms).await?;
            if row["facets"][format!("{PROFILE}DecisionRecord")]["decision"] == kind {
                job.status.decision_ref = Some(id.clone());
            }
            if let Some(request) = job.prepared.as_ref().and_then(|p| p.request.as_ref())
                && row["facets"][format!("{PROFILE}AttemptRecord")]["attempt_id"]
                    == request.attempt_id
            {
                job.status.attempt_ref = Some(id.clone());
            }
        }
        if job.status.decision_ref.is_none()
            || (job.prepared.as_ref().is_some_and(|p| p.request.is_some())
                && job.status.attempt_ref.is_none())
        {
            return Err("gate receipt outputs are incomplete".into());
        }
        Ok(())
    }

    pub(super) async fn confirm_parent(
        &self,
        session: &Session,
        parent: &str,
        scope: &RuntimeScope,
    ) -> Result<Job, BoxError> {
        let mut job = self
            .load(scope, parent)
            .await?
            .ok_or("continuation has no prepared parent")?;
        let wake = session.read_wake(DEFAULT_SPACE, parent).await?;
        if job.pins != wake.pins {
            return Err("parent journal configuration mismatch".into());
        }
        if !matches!(wake.state, WakeState::Completed { .. }) {
            return Err("parent gate has not committed".into());
        }
        // Exact replay also recovers native outputs when the directory ACK was
        // lost. It cannot mint a second decision or continuation.
        let needs_repair = job.status.state != "committed" || job.outputs.is_empty();
        self.commit(session, &wake, &mut job).await?;
        if needs_repair {
            self.save(&job).await?;
        }
        Ok(job)
    }
}
fn p_decision(job: &Job) -> Result<String, BoxError> {
    Ok(job
        .prepared
        .as_ref()
        .ok_or("gate preparation missing")?
        .decision
        .clone())
}
