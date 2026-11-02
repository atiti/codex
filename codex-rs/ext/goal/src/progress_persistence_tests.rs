use super::SavedReview;
use super::load;
use super::path;
use super::save;

#[test]
fn saves_and_restores_bounded_thread_state() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(dir.path(), "thread-1");
    save(
        &file,
        &SavedReview {
            active_seconds: 311,
            report_seconds: 400,
            previous: Some((5, "adjust: verify the release".into())),
        },
    )
    .unwrap();
    let restored = load(&file);
    assert_eq!(restored.active_seconds, 311);
    assert_eq!(restored.report_seconds, 400);
    assert_eq!(
        restored.previous,
        Some((5, "adjust: verify the release".into()))
    );
    save(
        &file,
        &SavedReview {
            active_seconds: 42,
            report_seconds: 0,
            previous: None,
        },
    )
    .unwrap();
    assert_eq!(load(&file).active_seconds, 42);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::write(&file, b"broken").unwrap();
    assert_eq!(load(&file).active_seconds, 0);
}

#[test]
fn loads_state_written_before_reporting_clock_existed() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(dir.path(), "legacy");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, br#"{"active_seconds":311,"previous":[5,"adjust"]}"#).unwrap();
    let restored = load(&file);
    assert_eq!(restored.active_seconds, 311);
    assert_eq!(restored.report_seconds, 0);
    assert_eq!(restored.previous, Some((5, "adjust".into())));
}

#[test]
fn restores_long_review_with_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let file = path(dir.path(), "thread");
    let message = format!(
        "Independent progress review [id]: adjust. {}\nAcknowledgement: accepted. {} Next action: {}",
        "x".repeat(1000),
        "r".repeat(500),
        "a".repeat(500)
    );
    save(
        &file,
        &SavedReview {
            active_seconds: 10,
            report_seconds: 20,
            previous: Some((7, message.clone())),
        },
    )
    .unwrap();
    assert_eq!(load(&file).previous, Some((7, message)));
}
