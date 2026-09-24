//! Shadow evaluation: a candidate memory policy run on a forked copy of a
//! Space, judged against the live one, without touching either.
//!
//! The fork primitive lives here too. A Space is forked by copying its
//! objects into a *different* store and opening that copy without background
//! autostart — the only supported way to get a throwaway copy, because AndaDB
//! metadata embeds its own base path and a rename inside one store would be
//! two Spaces reading one graph.

use super::*;

impl AppState {
    /// On-demand shadow evaluation (plan M11): forks the space twice —
    /// current policy vs candidate policy — settles both forks, replays
    /// recent real recall queries on each, and lets the judge blind-compare
    /// the answers (deterministically alternating A/B order to cancel
    /// position bias). The live space is only read: replays run on forks,
    /// so they can never pollute its conversations, usage ledger, or
    /// metrics (plan guardrail 4). Promotion stays human: read the report,
    /// then `update_space` with the candidate policy if it won.
    pub(crate) async fn run_shadow_eval(
        &self,
        space_id: &str,
        input: ShadowEvalInput,
    ) -> Result<ShadowReport, BoxError> {
        input.policy.validate()?;
        // Unpinned: shadow evaluation must not exempt a cold space from idle
        // eviction forever.
        let space = self.load_space(space_id, false).await?;
        // One shadow evaluation per space at a time: each run holds two full
        // in-memory copies of the space, so concurrent retries would stack
        // copies until the process OOMs (and race the `shadow_report` write).
        let Ok(_shadow_guard) = space.shadow_lock.try_lock() else {
            return Err("a shadow evaluation is already running for this space".into());
        };
        let now_ms = unix_ms();
        let sample = input
            .replay_sample
            .unwrap_or_else(|| space.memory_policy().shadow_replay_sample as usize)
            .clamp(1, 16);

        let queries = space.recent_recall_queries(sample).await?;
        if queries.is_empty() {
            return Err("no completed recall conversations to replay".into());
        }

        // Flush the live space (collections included, not just metadata) so
        // the forks see its latest persisted state, then fork twice:
        // baseline keeps the current policy, candidate gets the proposed
        // one. The fork is still not a point-in-time snapshot — writes that
        // land mid-fork may appear on one side and can produce false wins.
        // This legacy diagnostic is not admission evidence. Strict paired
        // experiments must use experiments::ExperimentSnapshot instead.
        // Settling both makes the comparison fair — same metabolism pass,
        // different knobs.
        space.flush().await.ok();
        let (baseline, candidate) = tokio::try_join!(
            self.fork_space(space_id, None),
            self.fork_space(space_id, Some(input.policy.clone())),
        )?;
        let _ = tokio::join!(
            baseline.settle_memory_metabolism(MaintenanceScope::Full, now_ms),
            candidate.settle_memory_metabolism(MaintenanceScope::Full, now_ms),
        );

        let mut report = ShadowReport {
            compared_at: now_ms,
            candidate_policy: input.policy,
            ..Default::default()
        };
        for (index, query) in queries.iter().enumerate() {
            let recall_input = || {
                StringOr::Value(RecallInput {
                    budget: None,
                    query: query.clone(),
                    context: None,
                })
            };
            let (baseline_out, candidate_out) = tokio::join!(
                baseline.query(SELF_USER_ID, recall_input()),
                candidate.query(SELF_USER_ID, recall_input()),
            );
            let (baseline_answer, candidate_answer) = match (baseline_out, candidate_out) {
                (Ok(baseline_out), Ok(candidate_out)) => {
                    report.usage.accumulate(&baseline_out.usage);
                    report.usage.accumulate(&candidate_out.usage);
                    (baseline_out.content, candidate_out.content)
                }
                _ => {
                    report.judge_errors += 1;
                    report.samples.push(ShadowSample {
                        query: crate::assess::truncate_chars(query, 200),
                        winner: "error".to_string(),
                        reason: "replay failed on one side".to_string(),
                    });
                    continue;
                }
            };
            report.replayed += 1;

            // Deterministic order alternation cancels position bias without
            // sacrificing reproducibility.
            let swap = index % 2 == 1;
            let (answer_a, answer_b) = if swap {
                (&candidate_answer, &baseline_answer)
            } else {
                (&baseline_answer, &candidate_answer)
            };
            let prompt = format!(
                "# User query\n{query}\n\n# Answer A\n{answer_a}\n\n# Answer B\n{answer_b}"
            );
            let verdict = crate::assess::AssessContext::judge_complete(
                space.as_ref(),
                anda_core::CompletionRequest {
                    instructions: SHADOW_JUDGE_INSTRUCTIONS.to_string(),
                    prompt,
                    effort: Some(anda_core::ModelEffort::Low),
                    ..Default::default()
                },
            )
            .await
            .and_then(|output| {
                report.usage.accumulate(&output.usage);
                crate::assess::parse_json_payload::<ShadowVerdict>(&output.content)
            });

            let (winner, reason) = match verdict {
                Ok(verdict) => {
                    let winner = match (verdict.winner.trim().to_lowercase().as_str(), swap) {
                        ("a", false) | ("b", true) => {
                            report.baseline_wins += 1;
                            "baseline"
                        }
                        ("b", false) | ("a", true) => {
                            report.candidate_wins += 1;
                            "candidate"
                        }
                        _ => {
                            report.ties += 1;
                            "tie"
                        }
                    };
                    (winner.to_string(), verdict.reason)
                }
                Err(err) => {
                    report.judge_errors += 1;
                    ("error".to_string(), err.to_string())
                }
            };
            report.samples.push(ShadowSample {
                query: crate::assess::truncate_chars(query, 200),
                winner,
                reason,
            });
        }

        // Forks live in memory and vanish on drop; closing is best-effort.
        let _ = baseline.close().await;
        let _ = candidate.close().await;

        space
            .db
            .set_extension_from("shadow_report".to_string(), report.clone());
        space.db.flush_metadata(unix_ms()).await.ok();
        Ok(report)
    }

    /// A sibling `AppState` over a different object store, sharing model and
    /// management configuration but with an empty space cache. Used by the
    /// shadow diagnostics and isolated experiments.
    pub fn fork_with_store(&self, object_store: Arc<dyn ObjectStore>) -> AppState {
        AppState {
            spaces: Arc::new(RwLock::new(BTreeMap::new())),
            attention_directory: crate::attention::Directory::new(
                object_store.clone(),
                self.sharding,
            ),
            attention_policy: self.attention_policy.clone(),
            action_bindings: None,
            memory_runtime_bindings: Arc::new(Default::default()),
            object_store,
            db_config: self.db_config.clone(),
            http_client: self.http_client.clone(),
            models: self.models.clone(),
            judge_model: self.judge_model.clone(),
            prompts: self.prompts.clone(),
            clock: self.clock.clone(),
            automatic: false,
            llm_semaphore: self.llm_semaphore.clone(),
            llm_request_semaphore: self.llm_request_semaphore.clone(),
            ed25519_pubkeys: self.ed25519_pubkeys.clone(),
            management: self.management.clone(),
            app_name: self.app_name.clone(),
            app_version: self.app_version.clone(),
            sharding: self.sharding,
        }
    }
}

const SHADOW_JUDGE_INSTRUCTIONS: &str = r#"You compare two answers an AI memory system gave to the same user query under two different internal configurations. Pick the answer that better serves the user: correct use of remembered facts, honoring later corrections, honest uncertainty. Ignore style differences.

Respond with ONLY a JSON object: {"winner": "a" | "b" | "tie", "reason": "..."}"#;

#[derive(Debug, serde::Deserialize)]
struct ShadowVerdict {
    winner: String,
    #[serde(default)]
    reason: String,
}

/// Copies every object of a space (`{space_id}/**`) from one object store to
/// another, preserving paths. This is the diagnostic fork primitive: AndaDB
/// metadata embeds its own base path, so a space must keep its id and be
/// forked into a *different* store — never renamed inside the same store.
pub(super) async fn copy_space_objects(
    src: &Arc<dyn ObjectStore>,
    dst: &Arc<dyn ObjectStore>,
    space_id: &str,
) -> Result<u64, BoxError> {
    use futures::TryStreamExt;
    use object_store::ObjectStoreExt;

    let prefix = object_store::path::Path::from(space_id);
    let learning_prefix = format!("{space_id}/learning/");
    let mut objects = src.list(Some(&prefix));
    let mut copied = 0u64;
    while let Some(meta) = objects.try_next().await? {
        // Learning journals contain live dispatch identity, not factual memory.
        // The caller also rejects an already configured source; this filter
        // closes the small configure-during-copy window without adding a
        // cross-component lock.
        if meta.location.as_ref().starts_with(&learning_prefix) {
            continue;
        }
        let payload = src.get(&meta.location).await?.bytes().await?;
        dst.put(&meta.location, payload.into()).await?;
        copied += 1;
    }
    if copied == 0 {
        return Err(format!("space {space_id} has no objects to copy").into());
    }
    Ok(copied)
}
