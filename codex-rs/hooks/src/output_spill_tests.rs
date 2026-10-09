use super::*;
use anyhow::Context;
use anyhow::Result;
use tempfile::tempdir;

#[test]
fn hook_output_spiller_is_scoped_to_its_thread() {
    let thread_id = ThreadId::new();
    let spiller = HookOutputSpiller::new(thread_id);

    assert_eq!(
        spiller.output_dir,
        AbsolutePathBuf::resolve_path_against_base(std::env::temp_dir(), "/")
            .join(HOOK_OUTPUTS_DIR)
            .join(thread_id.to_string())
    );
}

#[tokio::test]
async fn small_hook_output_remains_inline() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller {
        output_dir: output_dir.clone(),
    };

    let output = spiller.maybe_spill_text("short".to_string()).await;

    assert_eq!(output, "short");
    assert!(!output_dir.exists());
    Ok(())
}

#[tokio::test]
async fn large_hook_output_spills_to_file() -> Result<()> {
    let dir = tempdir()?;
    let text = "hook output ".repeat(1_000);
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller { output_dir };

    let output = spiller.maybe_spill_text(text.clone()).await;

    assert!(output.contains("tokens truncated"));
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix("Full hook output saved to: "))
        .context("spill path")?;
    assert_eq!(fs::read_to_string(path).await?, text);
    Ok(())
}

#[tokio::test]
async fn additional_contexts_apply_limits_individually() -> Result<()> {
    let dir = tempdir()?;
    let limited_text = "limited hook output ".repeat(1_000);
    let unlimited_text = "unlimited hook output ".repeat(5_000);
    let zero_limit_inline_text = "zero limit inline hook output ".repeat(800);
    let high_limit_inline_text = "high limit inline hook output ".repeat(800);
    assert!(approx_token_count(&unlimited_text) > MAX_HOOK_ADDITIONAL_CONTEXT_TOKEN_LIMIT);
    assert!(
        approx_token_count(&zero_limit_inline_text) > DEFAULT_HOOK_ADDITIONAL_CONTEXT_TOKEN_LIMIT
    );
    assert!(approx_token_count(&zero_limit_inline_text) < MAX_HOOK_ADDITIONAL_CONTEXT_TOKEN_LIMIT);
    assert!(
        approx_token_count(&high_limit_inline_text) > DEFAULT_HOOK_ADDITIONAL_CONTEXT_TOKEN_LIMIT
    );
    assert!(approx_token_count(&high_limit_inline_text) < MAX_HOOK_ADDITIONAL_CONTEXT_TOKEN_LIMIT);
    let output_dir = AbsolutePathBuf::from_absolute_path(dir.path())?.join(HOOK_OUTPUTS_DIR);
    let spiller = HookOutputSpiller { output_dir };
    let output = spiller
        .maybe_spill_additional_contexts(vec![
            AdditionalContext {
                text: limited_text.clone(),
                limit: AdditionalContextLimit::from_config(Some(1)),
            },
            AdditionalContext {
                text: unlimited_text.clone(),
                limit: AdditionalContextLimit::from_config(Some(0)),
            },
            AdditionalContext {
                text: unlimited_text.clone(),
                limit: AdditionalContextLimit::from_config(Some(usize::MAX)),
            },
            AdditionalContext {
                text: zero_limit_inline_text.clone(),
                limit: AdditionalContextLimit::from_config(Some(0)),
            },
            AdditionalContext {
                text: high_limit_inline_text.clone(),
                limit: AdditionalContextLimit::from_config(Some(usize::MAX)),
            },
        ])
        .await;
    let [
        limited_output,
        zero_limit_output,
        high_limit_output,
        zero_limit_inline_output,
        high_limit_inline_output,
    ] = output.as_slice()
    else {
        panic!("expected one output for each additional context");
    };
    assert!(limited_output.contains("Full hook output saved to:"));
    assert_ne!(zero_limit_output, &unlimited_text);
    assert_ne!(high_limit_output, &unlimited_text);
    assert!(approx_token_count(zero_limit_output) <= MAX_HOOK_ADDITIONAL_CONTEXT_TOKEN_LIMIT);
    assert!(approx_token_count(high_limit_output) <= MAX_HOOK_ADDITIONAL_CONTEXT_TOKEN_LIMIT);
    for spilled_output in [zero_limit_output, high_limit_output] {
        let path = spilled_output
            .lines()
            .find_map(|line| line.strip_prefix("Full hook output saved to: "))
            .context("spill path")?;
        assert_eq!(fs::read_to_string(path).await?, unlimited_text);
    }
    assert_eq!(zero_limit_inline_output, &zero_limit_inline_text);
    assert_eq!(high_limit_inline_output, &high_limit_inline_text);
    Ok(())
}

#[tokio::test]
async fn failed_hook_output_spill_stays_within_the_model_context_limit() -> Result<()> {
    let dir = tempdir()?;
    let output_dir = dir.path().join("not-a-directory");
    fs::write(&output_dir, "occupy the output directory path").await?;
    let spiller = HookOutputSpiller {
        output_dir: AbsolutePathBuf::from_absolute_path(&output_dir)?,
    };
    let text = "hook output ".repeat(20_000);
    let output = spiller
        .maybe_spill_text_with_limit(text, AdditionalContextLimit::from_config(Some(0)))
        .await;

    assert!(approx_token_count(&output) <= MAX_HOOK_ADDITIONAL_CONTEXT_TOKEN_LIMIT);
    Ok(())
}
