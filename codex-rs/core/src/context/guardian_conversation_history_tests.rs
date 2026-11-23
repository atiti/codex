use super::DEFAULT_GUARDIAN_HISTORY_PROMPT;
use super::GuardianConversationHistory;
use super::MAX_GUARDIAN_HISTORY_PROMPT_TOKENS;
use codex_utils_string::approx_bytes_for_tokens;
use codex_utils_string::approx_token_count;

#[test]
fn guardian_history_prompt_default_fits_its_context_budget() {
    let fragment = GuardianConversationHistory::new(/*prompt*/ None).expect("default prompt fits");

    assert_eq!(fragment.prompt, DEFAULT_GUARDIAN_HISTORY_PROMPT);
    assert!(approx_token_count(fragment.prompt) <= MAX_GUARDIAN_HISTORY_PROMPT_TOKENS);
}

#[test]
fn guardian_history_prompt_accepts_the_budget_and_rejects_oversized_overrides() {
    let within_budget = "x".repeat(approx_bytes_for_tokens(MAX_GUARDIAN_HISTORY_PROMPT_TOKENS));
    let oversized = "x".repeat(approx_bytes_for_tokens(
        MAX_GUARDIAN_HISTORY_PROMPT_TOKENS + 1,
    ));

    assert!(GuardianConversationHistory::new(Some(&within_budget)).is_ok());
    assert!(GuardianConversationHistory::new(Some(&oversized)).is_err());
}
