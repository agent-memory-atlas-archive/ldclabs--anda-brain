//! How an eval run reads on a terminal.
//!
//! The eval machinery answers with report structs; a person running
//! `anda_brain eval` wants a dozen lines. That rendering is neither the CLI's
//! (nothing about it depends on how the arguments were parsed) nor the
//! runner's (a report is a value, and what a caller does with it is not the
//! runner's business), and while it sat in the binary it could only be
//! exercised by running an eval.
//!
//! So it lives here, next to the reports it renders: pure functions from a
//! report to a string, with no clock, no filesystem and no process. The one
//! entry point a caller needs is [`EvalCommandReport`], which holds whichever
//! of the three report shapes a run produced and knows how to score, gate,
//! serialize and summarize it without the caller matching on which it is.
//!
//! Everything here is `pub` only because the `anda_brain` binary is a separate
//! crate and cannot reach `pub(crate)` items of its own library. It is the
//! CLI's rendering, not a stability surface — hence `#[doc(hidden)]` on the
//! module: how `anda_brain eval` prints a summary must stay free to change
//! without a breaking release of the library.

use std::fmt::Write;

use anda_core::Usage;

use crate::eval::{
    AttributionSummary, EvalExperimentReport, EvalGate, EvalGateReport, EvalReport, EvalScore,
    EvalSuiteReport, EvalValidationReport, EvalValidationSeverity, optimize::OptimizeReport,
};

pub enum EvalCommandReport {
    Scenario(EvalReport),
    Suite(EvalSuiteReport),
    Experiment(EvalExperimentReport),
}

impl EvalCommandReport {
    fn score_parts(&self) -> (&EvalScore, &AttributionSummary) {
        match self {
            Self::Scenario(report) => (&report.score, &report.attribution),
            Self::Suite(report) => (&report.score, &report.attribution),
            Self::Experiment(report) => (&report.score, &report.attribution),
        }
    }

    pub fn evaluate_gate(&self, gate: &EvalGate) -> EvalGateReport {
        let (score, attribution) = self.score_parts();
        gate.evaluate(score, attribution)
    }

    pub fn attach_gate_report(&mut self, gate_report: EvalGateReport) {
        match self {
            Self::Scenario(report) => report.gate = Some(gate_report),
            Self::Suite(report) => report.gate = Some(gate_report),
            Self::Experiment(report) => report.gate = Some(gate_report),
        }
    }

    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Scenario(report) => serde_json::to_string_pretty(report),
            Self::Suite(report) => serde_json::to_string_pretty(report),
            Self::Experiment(report) => serde_json::to_string_pretty(report),
        }
    }

    pub fn to_summary(&self, gate_report: Option<&EvalGateReport>) -> String {
        let mut out = String::new();
        match self {
            Self::Scenario(report) => {
                writeln!(out, "Eval scenario {}", report.scenario_id).ok();
                append_score_summary(&mut out, &report.score);
                append_stddev_summary(&mut out, report.total_stddev);
                append_attribution_summary(&mut out, &report.attribution);
                append_usage_summary(&mut out, &report.usage);
                if !report.satisfaction_trajectory.is_empty() {
                    let trajectory: Vec<String> = report
                        .satisfaction_trajectory
                        .iter()
                        .map(|point| format!("{}:{:.2}", point.turn, point.satisfaction))
                        .collect();
                    writeln!(out, "satisfaction: {}", trajectory.join(" ")).ok();
                }
                writeln!(out, "turns: {}", report.turns.len()).ok();
            }
            Self::Suite(report) => {
                writeln!(out, "Eval suite {}", report.suite_id).ok();
                append_score_summary(&mut out, &report.score);
                append_stddev_summary(&mut out, report.total_stddev);
                append_attribution_summary(&mut out, &report.attribution);
                append_usage_summary(&mut out, &report.usage);
                writeln!(out, "scenarios: {}", report.reports.len()).ok();
                for scenario in &report.reports {
                    writeln!(
                        out,
                        "- {} total={:.4} findings={}",
                        scenario.scenario_id,
                        scenario.score.total,
                        scenario.attribution.total_findings()
                    )
                    .ok();
                }
            }
            Self::Experiment(report) => {
                writeln!(out, "Eval experiment {}", report.experiment_id).ok();
                append_score_summary(&mut out, &report.score);
                append_stddev_summary(&mut out, report.total_stddev);
                append_attribution_summary(&mut out, &report.attribution);
                append_usage_summary(&mut out, &report.usage);
                if !report.shared_formation.is_empty() {
                    let usage: u64 = report
                        .shared_formation
                        .iter()
                        .map(|shared| {
                            shared
                                .usage
                                .input_tokens
                                .saturating_add(shared.usage.output_tokens)
                        })
                        .sum();
                    writeln!(
                        out,
                        "shared_formation: {} scenario(s), {} tokens (excluded from suites)",
                        report.shared_formation.len(),
                        usage
                    )
                    .ok();
                }
                if let Some(best_suite_id) = &report.best_suite_id {
                    writeln!(out, "best_suite: {best_suite_id}").ok();
                }
                writeln!(out, "suites: {}", report.suites.len()).ok();
                for comparison in &report.comparisons {
                    writeln!(
                        out,
                        "- #{} {} total={:.4} delta={:.4} findings={} tokens={}",
                        comparison.rank,
                        comparison.suite_id,
                        comparison.score.total,
                        comparison.delta_from_best_total,
                        comparison.total_findings,
                        comparison.total_tokens
                    )
                    .ok();
                }
            }
        }

        if let Some(gate_report) = gate_report {
            append_gate_summary(&mut out, gate_report);
        }
        out
    }
}

pub fn eval_validation_error(report: &EvalValidationReport) -> String {
    let errors: Vec<String> = report
        .issues
        .iter()
        .filter(|issue| issue.severity == EvalValidationSeverity::Error)
        .take(5)
        .map(|issue| format!("{}: {}", issue.path, issue.message))
        .collect();

    if errors.is_empty() {
        "eval validation failed".to_string()
    } else {
        format!("eval validation failed: {}", errors.join("; "))
    }
}

pub fn validation_summary(report: &EvalValidationReport) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "Eval validation {}",
        if report.passed { "passed" } else { "failed" }
    )
    .ok();
    writeln!(out, "planned_runs: {}", report.planned_runs).ok();
    writeln!(out, "scenarios: {}", report.scenarios.len()).ok();
    for scenario in &report.scenarios {
        writeln!(
            out,
            "- {} normal={} checkpoint={} maintenance={} memories={} probes={} simulated={} noise={} assertions={}",
            scenario.id,
            scenario.normal_turns,
            scenario.checkpoint_turns,
            scenario.maintenance_turns,
            scenario.expected_memories,
            scenario.probes,
            scenario.simulated_turns,
            scenario.noise_turns,
            scenario.assertions
        )
        .ok();
    }
    writeln!(out, "profiles: {}", report.profiles.len()).ok();
    for profile in &report.profiles {
        let cadence = profile
            .maintenance_every_n_turns
            .map(|turns| format!("every_{turns}_turns"))
            .unwrap_or_else(|| "manual".to_string());
        writeln!(
            out,
            "- {} maintenance={} scope={} timeout_ms={} poll_ms={} samples={} judge={:?}",
            profile.id,
            cadence,
            profile.maintenance_scope,
            profile.wait_timeout_ms,
            profile.poll_interval_ms,
            profile.checkpoint_samples,
            profile.judge
        )
        .ok();
    }
    append_validation_issues_summary(&mut out, report);
    out
}

fn append_validation_issues_summary(out: &mut String, report: &EvalValidationReport) {
    let errors = report
        .issues
        .iter()
        .filter(|issue| issue.severity == EvalValidationSeverity::Error)
        .count();
    let warnings = report.issues.len().saturating_sub(errors);
    writeln!(out, "issues: errors={errors} warnings={warnings}").ok();
    for issue in &report.issues {
        writeln!(
            out,
            "- {:?} {}: {}",
            issue.severity, issue.path, issue.message
        )
        .ok();
    }
}

fn append_score_summary(out: &mut String, score: &EvalScore) {
    writeln!(
        out,
        "score: total={:.4} memory={:.4} evolution={:.4} uncertainty={:.4} forgetting={:.4} graph={:.4} latency_penalty={:.4} token_penalty={:.4}",
        score.total,
        score.memory_utility,
        score.evolution_quality,
        score.uncertainty_calibration,
        score.forgetting_quality,
        score.graph_health,
        score.latency_penalty,
        score.token_cost_penalty
    )
    .ok();
}

fn append_stddev_summary(out: &mut String, total_stddev: Option<f64>) {
    if let Some(stddev) = total_stddev {
        writeln!(out, "total_stddev: {stddev:.4}").ok();
    }
}

fn append_attribution_summary(out: &mut String, attribution: &AttributionSummary) {
    writeln!(
        out,
        "findings: total={} formation_miss={} bad_consolidation={} bad_grounding={} bad_synthesis={} overconfidence={} graph_probe_error={} latency_cost={} token_cost={} judge_error={}",
        attribution.total_findings(),
        attribution.formation_miss,
        attribution.bad_consolidation,
        attribution.bad_grounding,
        attribution.bad_synthesis,
        attribution.overconfidence,
        attribution.graph_probe_error,
        attribution.latency_cost,
        attribution.token_cost,
        attribution.judge_error
    )
    .ok();
}

fn append_usage_summary(out: &mut String, usage: &Usage) {
    writeln!(
        out,
        "usage: input_tokens={} output_tokens={} cached_tokens={} requests={}",
        usage.input_tokens, usage.output_tokens, usage.cached_tokens, usage.requests
    )
    .ok();
}

fn append_gate_summary(out: &mut String, gate_report: &EvalGateReport) {
    writeln!(
        out,
        "gate: {} min_score={} max_findings={}",
        if gate_report.passed {
            "passed"
        } else {
            "failed"
        },
        gate_report
            .criteria
            .min_total_score
            .map(|score| format!("{score:.4}"))
            .unwrap_or_else(|| "none".to_string()),
        gate_report
            .criteria
            .max_total_findings
            .map(|findings| findings.to_string())
            .unwrap_or_else(|| "none".to_string())
    )
    .ok();
    for failure in &gate_report.failures {
        writeln!(out, "- {failure}").ok();
    }
}

pub fn optimize_summary(report: &OptimizeReport, out_dir: &str) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "Optimize: baseline={:.4} final={:.4} accepted={}/{}",
        report.baseline_total,
        report.final_total,
        report.accepted_generations,
        report.generations.len()
    )
    .ok();
    for generation in &report.generations {
        let holdout = generation
            .holdout_total
            .map(|total| format!(" holdout={total:.4}"))
            .unwrap_or_default();
        writeln!(
            out,
            "- gen {} target={} candidate={}{holdout} {} ({})",
            generation.generation,
            generation
                .target
                .map(|target| target.as_str())
                .unwrap_or("policy"),
            generation
                .candidate_total
                .map(|total| format!("{total:.4}"))
                .unwrap_or_else(|| "-".to_string()),
            if generation.decision.accepted {
                "accepted"
            } else {
                "rejected"
            },
            generation.decision.reason
        )
        .ok();
    }
    if !report.accepted_prompts.is_empty() {
        writeln!(out, "accepted prompts written to {out_dir}").ok();
    }
    if report.accepted_policy.is_some() {
        writeln!(
            out,
            "accepted policy written to {out_dir}/memory_policy.json"
        )
        .ok();
    }
    out
}
