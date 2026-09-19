use std::time::{Duration, Instant};

use gateway_core::account::{
    AccountAttemptFeedback, AccountCandidate, AccountFeedbackStats, AccountSelector,
    ProviderAccountId, RotationStrategy,
};
use gateway_core::routing::ProviderKind;

use super::{candidate, candidate_with_concurrency, context, weighted_candidate};

#[test]
fn smart_selector_should_prioritize_ready_routing_state_before_score() {
    let mut candidates = [
        candidate("acct_missing", 0, Some(10_000)),
        candidate("acct_ready", 2, Some(100)),
    ];
    candidates[0].routing_state_ready = Some(false);
    candidates[1].routing_state_ready = Some(true);
    candidates[1].signals.failure_rate_basis_points = Some(5_000);
    candidates[1].signals.first_output_latency_ms = Some(20_000);
    assert!(
        smart_selection_ids(&candidates)
            .iter()
            .all(|id| *id == "acct_ready")
    );
}

#[test]
fn smart_selector_should_keep_scoring_and_rotation_within_ready_accounts() {
    let mut candidates = [
        candidate("acct_a", 0, None),
        candidate("acct_b", 0, None),
        candidate("acct_loaded", 2, None),
        candidate("acct_missing", 0, Some(10_000)),
    ];
    for candidate in &mut candidates {
        candidate.routing_state_ready = Some(candidate.account.id().as_str() != "acct_missing");
    }
    let ids = smart_selection_ids(&candidates);
    assert_eq!(ids, ["acct_a", "acct_b"].repeat(10));
}

#[test]
fn smart_selector_should_fall_back_when_ready_accounts_are_blocked() {
    for blocker in [
        "concurrency",
        "interval",
        "excluded",
        "cooldown",
        "disabled",
    ] {
        let mut candidates = [
            candidate("acct_ready", 0, None),
            candidate("acct_fallback", 0, None),
        ];
        candidates[0].routing_state_ready = Some(true);
        candidates[1].routing_state_ready = Some(false);
        let mut context = context(RotationStrategy::Smart);
        match blocker {
            "concurrency" => candidates[0].signals.in_flight = 3,
            "interval" => {
                candidates[0].signals.last_started_at = Some(context.now + Duration::from_secs(1))
            }
            "excluded" => {
                context
                    .excluded_accounts
                    .insert(candidates[0].account.id().clone());
            }
            "cooldown" => {
                candidates[0].signals.cooldown =
                    Some((context.now + Duration::from_secs(60)).into())
            }
            "disabled" => {
                candidates[0].account = candidates[0].account.clone().with_account_facts(
                    false,
                    gateway_core::account::CredentialState::Ready,
                    gateway_core::account::QuotaState::unknown(),
                    None,
                    None,
                )
            }
            _ => unreachable!(),
        }
        let selected = AccountSelector
            .select(&candidates, &context)
            .expect("fallback");
        assert_eq!(
            selected.candidate().account.id().as_str(),
            "acct_fallback",
            "{blocker}"
        );
    }
}

#[test]
fn routing_state_should_not_override_weight_or_affinity() {
    let mut candidates = [
        weighted_candidate("acct_high", 100, 0),
        weighted_candidate("acct_ready", 1, 0),
    ];
    candidates[0].routing_state_ready = Some(false);
    candidates[1].routing_state_ready = Some(true);
    let mut context = context(RotationStrategy::Smart);
    assert_eq!(
        AccountSelector
            .select(&candidates, &context)
            .expect("highest weight")
            .candidate()
            .account
            .id()
            .as_str(),
        "acct_high"
    );

    candidates[0].account = weighted_candidate("acct_high", 1, 0).account;
    context.preferred_account = Some(candidates[0].account.id().clone());
    let selected = AccountSelector
        .select(&candidates, &context)
        .expect("affinity");
    assert_eq!(selected.candidate().account.id().as_str(), "acct_high");
    assert_eq!(
        selected.preferred(),
        gateway_core::account::PreferredAccountSelection::Hit
    );
}

#[test]
fn smart_selector_should_preserve_neutral_and_all_missing_routing_state_behavior() {
    let mut candidates = [candidate("acct_a", 0, None), candidate("acct_b", 0, None)];
    let baseline = smart_selection_ids(&candidates)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for states in [
        [Some(false), Some(false)],
        [Some(true), None],
        [Some(false), None],
    ] {
        for (candidate, state) in candidates.iter_mut().zip(states) {
            candidate.routing_state_ready = state;
        }
        assert_eq!(smart_selection_ids(&candidates), baseline, "{states:?}");
    }
}

#[test]
fn routing_state_should_not_change_other_rotation_strategies() {
    for strategy in [
        RotationStrategy::RoundRobin,
        RotationStrategy::Sticky,
        RotationStrategy::QuotaResetPriority,
    ] {
        let mut candidates = [candidate("acct_a", 0, None), candidate("acct_b", 0, None)];
        candidates[0].routing_state_ready = Some(false);
        candidates[1].routing_state_ready = Some(true);
        assert_eq!(
            AccountSelector
                .select(&candidates, &context(strategy))
                .expect("original order")
                .candidate()
                .account
                .id()
                .as_str(),
            "acct_a"
        );
    }
}

const FAILURE_RATE_HALF_LIFE: Duration = Duration::from_secs(15 * 60);

fn feedback_subject() -> (AccountFeedbackStats, ProviderKind, ProviderAccountId) {
    (
        AccountFeedbackStats::default(),
        ProviderKind::new("openai").expect("valid provider"),
        ProviderAccountId::new("acct_decay").expect("valid account"),
    )
}

fn report_failure(
    feedback: &AccountFeedbackStats,
    provider: &ProviderKind,
    account: &ProviderAccountId,
    observed_at: Instant,
) {
    feedback.report_at(
        provider,
        account,
        AccountAttemptFeedback::Failed {
            first_output_ms: None,
        },
        observed_at,
    );
}

#[test]
fn capacity_rejections_should_raise_failure_rate_faster_than_regular_failures() {
    let (feedback, provider, account) = feedback_subject();
    let observed_at = Instant::now();
    let mut failure_rates = Vec::new();
    for _ in 0..2 {
        feedback.report_at(
            &provider,
            &account,
            AccountAttemptFeedback::CapacityRejected {
                first_output_ms: None,
            },
            observed_at,
        );
        failure_rates.push(
            feedback
                .scheduling_signals_at(&provider, &account, observed_at)
                .0,
        );
    }

    assert_eq!(failure_rates, [Some(4_000), Some(6_400)]);
}

#[test]
fn capacity_failure_rate_should_keep_time_decay_and_success_recovery() {
    let (feedback, provider, account) = feedback_subject();
    let observed_at = Instant::now();
    feedback.report_at(
        &provider,
        &account,
        AccountAttemptFeedback::CapacityRejected {
            first_output_ms: Some(100),
        },
        observed_at,
    );
    let recovered_at = observed_at + FAILURE_RATE_HALF_LIFE;
    feedback.report_at(
        &provider,
        &account,
        AccountAttemptFeedback::Succeeded {
            first_output_ms: Some(200),
        },
        recovered_at,
    );

    assert_eq!(
        feedback.scheduling_signals_at(&provider, &account, recovered_at),
        (Some(1_600), Some(120)),
    );
}

#[test]
fn account_failure_rate_should_halve_after_one_half_life() {
    let (feedback, provider, account) = feedback_subject();
    let observed_at = Instant::now();
    report_failure(&feedback, &provider, &account, observed_at);

    let failure_rate = feedback
        .scheduling_signals_at(&provider, &account, observed_at + FAILURE_RATE_HALF_LIFE)
        .0;

    assert_eq!(failure_rate, Some(1_000));
}

#[test]
fn account_failure_rate_should_quarter_after_two_half_lives() {
    let (feedback, provider, account) = feedback_subject();
    let observed_at = Instant::now();
    report_failure(&feedback, &provider, &account, observed_at);

    let failure_rate = feedback
        .scheduling_signals_at(
            &provider,
            &account,
            observed_at + FAILURE_RATE_HALF_LIFE * 2,
        )
        .0;

    assert_eq!(failure_rate, Some(500));
}

#[test]
fn account_failure_rate_should_decay_before_applying_a_new_sample() {
    let (feedback, provider, account) = feedback_subject();
    let observed_at = Instant::now();
    report_failure(&feedback, &provider, &account, observed_at);
    feedback.report_at(
        &provider,
        &account,
        AccountAttemptFeedback::Succeeded {
            first_output_ms: None,
        },
        observed_at + FAILURE_RATE_HALF_LIFE,
    );

    let failure_rate = feedback
        .scheduling_signals_at(&provider, &account, observed_at + FAILURE_RATE_HALF_LIFE)
        .0;

    assert_eq!(failure_rate, Some(800));
}

#[test]
fn concurrent_account_failures_should_not_lose_samples() {
    let (feedback, provider, account) = feedback_subject();
    let observed_at = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| report_failure(&feedback, &provider, &account, observed_at));
        }
    });

    let failure_rate = feedback
        .scheduling_signals_at(&provider, &account, observed_at)
        .0;

    assert_eq!(failure_rate, Some(8_322));
}
fn smart_selection_ids(candidates: &[AccountCandidate]) -> Vec<&str> {
    let mut selection = context(RotationStrategy::Smart);
    (0..20)
        .map(|cursor| {
            selection.round_robin_cursor = cursor;
            AccountSelector
                .select(candidates, &selection)
                .expect("candidate available")
                .candidate()
                .account
                .id()
                .as_str()
        })
        .collect()
}

#[test]
fn smart_selector_should_rotate_despite_small_signal_differences() {
    for signal in ["quota", "latency", "load", "failure"] {
        let mut candidates = [
            candidate_with_concurrency("acct_a", 0, 100),
            candidate_with_concurrency("acct_b", 0, 100),
        ];
        match signal {
            "quota" => {
                candidates[0].signals.quota_remaining_rank = Some(600);
                candidates[1].signals.quota_remaining_rank = Some(601);
            }
            "latency" => {
                candidates[0].signals.first_output_latency_ms = Some(2_500);
                candidates[1].signals.first_output_latency_ms = Some(2_501);
            }
            "load" => candidates[0].signals.in_flight = 1,
            "failure" => candidates[0].signals.failure_rate_basis_points = Some(1),
            _ => unreachable!(),
        }
        let selected = smart_selection_ids(&candidates);

        assert_eq!(selected, ["acct_a", "acct_b"].repeat(10), "{signal}");
    }
}

#[test]
fn smart_selector_should_keep_rotation_order_when_nearby_scores_cross() {
    let mut candidates = [
        candidate("acct_a", 0, Some(8_000)),
        candidate("acct_b", 0, Some(8_001)),
        candidate("acct_worse", 0, Some(2_000)),
    ];
    let mut selection = context(RotationStrategy::Smart);
    let mut selected = Vec::new();
    for cursor in 0..20 {
        selection.round_robin_cursor = cursor;
        for candidate in &mut candidates {
            match candidate.account.id().as_str() {
                "acct_a" => candidate.signals.quota_remaining_rank = Some(8_000 + cursor % 2),
                "acct_b" => candidate.signals.quota_remaining_rank = Some(8_001 - cursor % 2),
                _ => {}
            }
        }
        candidates.reverse();
        selected.push(
            AccountSelector
                .select(&candidates, &selection)
                .expect("candidate available")
                .candidate()
                .account
                .id()
                .as_str()
                .to_owned(),
        );
    }

    assert_eq!(selected, ["acct_a", "acct_b"].repeat(10));
}

#[test]
fn smart_selector_should_only_rotate_among_candidates_close_to_the_best() {
    let candidates = [
        candidate("acct_a", 0, Some(10_000)),
        candidate("acct_b", 0, Some(9_500)),
        candidate("acct_c", 0, Some(9_000)),
    ];

    assert_eq!(
        smart_selection_ids(&candidates),
        ["acct_a", "acct_b"].repeat(10)
    );
}

#[test]
fn smart_selector_should_preserve_material_signal_advantages() {
    for signal in ["quota", "latency", "load", "failure"] {
        let mut candidates = [
            candidate_with_concurrency("acct_a", 0, 10),
            candidate_with_concurrency("acct_b", 0, 10),
        ];
        match signal {
            "quota" => {
                candidates[0].signals.quota_remaining_rank = Some(600);
                candidates[1].signals.quota_remaining_rank = Some(2_600);
            }
            "latency" => {
                candidates[0].signals.first_output_latency_ms = Some(20_000);
                candidates[1].signals.first_output_latency_ms = Some(2_500);
            }
            "load" => candidates[0].signals.in_flight = 1,
            "failure" => candidates[0].signals.failure_rate_basis_points = Some(2_000),
            _ => unreachable!(),
        }

        assert_eq!(smart_selection_ids(&candidates), ["acct_b"; 20], "{signal}");
    }
}

#[test]
fn smart_selector_should_balance_actual_load_against_remaining_quota() {
    let mut candidates = [
        candidate_with_concurrency("acct_a", 0, 10),
        candidate_with_concurrency("acct_b", 1, 10),
    ];
    candidates[0].signals.quota_remaining_rank = Some(600);
    candidates[1].signals.quota_remaining_rank = Some(2_600);

    assert_eq!(smart_selection_ids(&candidates), ["acct_b"; 20]);
}

#[test]
fn smart_selector_should_treat_unknown_quota_as_neutral() {
    let candidates = [
        candidate("acct_known_low", 0, Some(600)),
        candidate("acct_unknown", 0, None),
    ];

    assert_eq!(smart_selection_ids(&candidates), ["acct_unknown"; 20]);
}
