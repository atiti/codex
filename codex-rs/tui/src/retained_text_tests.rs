use super::*;
use pretty_assertions::assert_eq;

#[test]
fn large_output_is_spilled_shared_and_removed_after_last_owner() {
    let original = "é".repeat(INLINE_TEXT_BYTES);
    let text = RetainedText::new(original.clone());
    let Storage::File { file, .. } = text.0.as_ref() else {
        panic!("large text must spill");
    };
    let path = file.to_path_buf();
    let shared = text.clone();
    assert_eq!(shared.read(), original);
    drop(text);
    assert!(path.exists());
    drop(shared);
    assert!(!path.exists());
}

#[test]
fn small_output_remains_borrowed_and_exact() {
    let text = RetainedText::new("small\noutput".to_owned());
    assert!(matches!(text.read(), Cow::Borrowed(_)));
    assert_eq!(text.read(), "small\noutput");
}

#[test]
fn spilled_preview_keeps_utf8_head_and_tail_without_loading_full_output() {
    let source = format!("begin:{}:end", "🙂".repeat(INLINE_TEXT_BYTES));
    let text = RetainedText::new(source.clone());
    assert!(text.preview().len() < INLINE_TEXT_BYTES + 128);
    assert!(text.preview().starts_with("begin:"));
    assert!(text.preview().ends_with(":end"));
    assert!(text.preview().contains("retained on disk"));
    assert_eq!(text.read(), source);
}
