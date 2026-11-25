// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/cascade/cascade_io/rebalance/render.rs
//! Rendering for the fallback rebalance plan and apply outcome.
//!
//! Pure formatting: no effect, no observation. The parent module owns the
//! values; this module owns their stable text/JSON spelling.

use super::*;

#[derive(Serialize)]
struct PlanJson<'a> {
    schema: &'a str,
    target: TargetJson<'a>,
    higher_tiers: Vec<TierJson<'a>>,
    available_bytes: u64,
    remaining_swap_free_bytes: u64,
    reserve_bytes: u64,
    margin_bytes: u64,
    required_bytes: u64,
    deficit_bytes: u64,
    verdict: HeadroomVerdict,
    reason: &'a str,
    would_change: bool,
}

#[derive(Serialize)]
struct TargetJson<'a> {
    path: &'a str,
    uuid: &'a str,
    dev_t: &'a str,
    priority: i32,
    size_bytes: u64,
    used_bytes: u64,
    last_resort: bool,
}

#[derive(Serialize)]
struct TierJson<'a> {
    path: &'a str,
    role: &'a str,
    priority: i32,
    size_bytes: u64,
    used_bytes: u64,
}

pub(super) fn render_plan(plan: &RebalancePlan, json: bool) -> Result<String, CascadeError> {
    if json {
        let body = PlanJson {
            schema: plan.schema,
            target: TargetJson {
                path: &plan.target.path,
                uuid: &plan.target.uuid,
                dev_t: &plan.target.dev_t,
                priority: plan.target.priority,
                size_bytes: plan.target.size_bytes,
                used_bytes: plan.target.used_bytes,
                last_resort: plan.target.last_resort,
            },
            higher_tiers: plan
                .higher_tiers
                .iter()
                .map(|tier| TierJson {
                    path: &tier.path,
                    role: role_name(tier.role),
                    priority: tier.priority,
                    size_bytes: tier.size_bytes,
                    used_bytes: tier.used_bytes,
                })
                .collect(),
            available_bytes: plan.available_bytes,
            remaining_swap_free_bytes: plan.remaining_swap_free_bytes,
            reserve_bytes: plan.reserve_bytes,
            margin_bytes: plan.margin_bytes,
            required_bytes: plan.required_bytes,
            deficit_bytes: plan.deficit_bytes,
            verdict: plan.verdict,
            reason: plan.reason,
            would_change: plan.would_change,
        };
        return serde_json::to_string(&body)
            .map_err(|error| CascadeError::Io(format!("encode swap-rebalance plan: {error}")));
    }
    let mut text = format!(
        "schema={}\ntarget={} uuid={} dev={} priority={} size={} B used={} B last_resort={}\n",
        plan.schema,
        plan.target.path,
        plan.target.uuid,
        plan.target.dev_t,
        plan.target.priority,
        plan.target.size_bytes,
        plan.target.used_bytes,
        plan.target.last_resort
    );
    for tier in &plan.higher_tiers {
        text.push_str(&format!(
            "higher_tier={} role={} priority={} size={} B used={} B\n",
            tier.path,
            role_name(tier.role),
            tier.priority,
            tier.size_bytes,
            tier.used_bytes
        ));
    }
    text.push_str(&format!(
        "managed={} B\navailable={} B\nremaining_swap_free={} B (reported, not credited)\nreserve={} B\nmargin={} B\nrequired={} B\ndeficit={} B\nverdict={} reason={} would_change={}",
        plan.target.used_bytes,
        plan.available_bytes,
        plan.remaining_swap_free_bytes,
        plan.reserve_bytes,
        plan.margin_bytes,
        plan.required_bytes,
        plan.deficit_bytes,
        match plan.verdict {
            HeadroomVerdict::Safe => "safe",
            HeadroomVerdict::Blocked => "blocked",
        },
        plan.reason,
        plan.would_change
    ));
    Ok(text)
}

pub(super) fn render_outcome(outcome: &RebalanceOutcome) -> String {
    // `Ok` here means "the command ran and returned success", not "a command
    // ran". An already-balanced target is decided before the effect seam and
    // must not claim a swapoff/swapon it never issued.
    let no_op = outcome.result == "already_balanced";
    let swapoff = match (outcome.swapoff_error.as_deref(), no_op) {
        (Some(error), _) => error,
        (None, true) => "not_run",
        (None, false) => "ok",
    };
    let swapon = match (outcome.swapon_error.as_deref(), no_op) {
        (Some(error), _) => error,
        (None, true) => "not_run",
        (None, false) => "ok",
    };
    format!(
        "fallback={} uuid={} priority={} used_before={} B\ntarget_after_used={} B\nswapoff={} swapon={} restored={} applied={}\nresult={}",
        outcome.target_before.path,
        outcome.target_before.uuid,
        outcome.target_before.priority,
        outcome.target_before.used_bytes,
        match outcome.target_after_used_bytes {
            Some(bytes) => bytes.to_string(),
            None => "unverified".to_string(),
        },
        swapoff,
        swapon,
        outcome.restored,
        outcome.applied,
        outcome.result
    )
}
