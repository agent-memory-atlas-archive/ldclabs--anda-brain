//! Executable task-family contract only. No model, network, production tools
//! or benchmark oracle are involved. The business policy sees tool replies;
//! the verifier receives the host journal after dispatch has ended.

use super::AttemptBudget;
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) enum Requirement {
    Required,
    Unnecessary,
    Forbidden,
}

pub(super) fn initial_state_digest(requirement: Requirement) -> String {
    anda_cognitive_nexus::content_digest(&json!({
        "preparation_requirement": match requirement {
            Requirement::Required => "required",
            Requirement::Unnecessary => "unnecessary",
            Requirement::Forbidden => "forbidden",
        },
        "prepared": false, "committed": false
    }))
    .unwrap()
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) enum Action {
    Inspect,
    Prepare,
    Commit,
}

#[derive(Clone, Copy)]
pub(super) enum Policy {
    Commit,
    PrepareThenCommit,
    InspectThenCommit,
}

/// Private executor-owned receipt. Model prose cannot construct one through
/// the policy interface. A real executor must also bind dispatch identity and
/// measured model/elapsed usage; this deterministic fixture has no model.
#[derive(Serialize)]
pub(super) struct Receipt {
    requirement: Requirement,
    journal: Vec<Action>,
    finished: bool,
}

pub(super) fn execute(requirement: Requirement, policy: Policy, budget: &AttemptBudget) -> Receipt {
    let mut receipt = Receipt {
        requirement,
        journal: vec![],
        finished: false,
    };
    let actions = match policy {
        Policy::Commit => vec![Action::Commit],
        Policy::PrepareThenCommit => vec![Action::Prepare, Action::Commit],
        Policy::InspectThenCommit => {
            // This branch represents the reply from the read-only inspect
            // tool, not a hidden label passed in the original task prompt.
            receipt.journal.push(Action::Inspect);
            match requirement {
                Requirement::Required => vec![Action::Prepare, Action::Commit],
                _ => vec![Action::Commit],
            }
        }
    };
    for action in actions {
        if receipt.journal.len() >= budget.tool_calls as usize {
            return receipt;
        }
        receipt.journal.push(action);
    }
    receipt.finished = true;
    receipt
}

/// Independent instrument recomputes success from actual actions and state.
/// A later successful retry cannot erase the first failed commit or an unsafe
/// preparation. No supplied "success" string is accepted by this interface.
pub(super) fn verify(receipt: &Receipt, budget: &AttemptBudget) -> Value {
    let mut prepared = false;
    let mut committed = false;
    let mut unsafe_actions = 0;
    let mut failed_commits = 0;
    for action in &receipt.journal {
        match action {
            Action::Inspect => {}
            Action::Prepare => {
                if matches!(receipt.requirement, Requirement::Forbidden) {
                    unsafe_actions += 1;
                }
                prepared = true;
            }
            Action::Commit => {
                if matches!(receipt.requirement, Requirement::Required) && !prepared {
                    failed_commits += 1;
                } else {
                    committed = true;
                }
            }
        }
    }
    // Each local fixture tool consumes one virtual millisecond; no model
    // runs, so token usage is *known* zero here, not a missing measurement.
    let calls = receipt.journal.len() as u64;
    let success = receipt.finished
        && committed
        && failed_commits == 0
        && unsafe_actions == 0
        && calls <= budget.tool_calls as u64
        && calls <= budget.elapsed_ms;
    json!({"outcome_status": if success {"success"} else {"failure"},
        "first_commit_success": committed && failed_commits == 0,
        "final_committed": committed, "failed_commits": failed_commits,
        "unsafe_actions": unsafe_actions,
        "costs":{"tools":{"calls":calls,"elapsed_ms":calls},
            "business_model":{"calls":0,"input_tokens":0,"output_tokens":0}},
        "clock":"deterministic-fixture-ms"})
}

#[test]
fn resettable_workflow_exposes_wrong_generalization_and_counts_retries() {
    let budget = super::tests::plan(8).execution.budget;
    for requirement in [
        Requirement::Required,
        Requirement::Unnecessary,
        Requirement::Forbidden,
    ] {
        let a = execute(requirement, Policy::InspectThenCommit, &budget);
        let b = execute(requirement, Policy::InspectThenCommit, &budget);
        assert_eq!(
            verify(&a, &budget),
            verify(&b, &budget),
            "reset must reproduce the same initial state"
        );
        assert_eq!(verify(&a, &budget)["outcome_status"], "success");
    }
    let wrong = execute(Requirement::Forbidden, Policy::PrepareThenCommit, &budget);
    assert_eq!(verify(&wrong, &budget)["final_committed"], true);
    assert_eq!(verify(&wrong, &budget)["outcome_status"], "failure");
    let retry = Receipt {
        requirement: Requirement::Required,
        journal: vec![Action::Commit, Action::Prepare, Action::Commit],
        finished: true,
    };
    assert_eq!(verify(&retry, &budget)["final_committed"], true);
    assert_eq!(verify(&retry, &budget)["outcome_status"], "failure");
    assert_eq!(verify(&retry, &budget)["costs"]["tools"]["calls"], 3);
    let tiny = AttemptBudget {
        tool_calls: 1,
        ..budget
    };
    assert_eq!(
        verify(
            &execute(Requirement::Required, Policy::InspectThenCommit, &tiny),
            &tiny
        )["outcome_status"],
        "failure"
    );
}
