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
            previous: Some((5, "adjust: verify the release".into())),
        },
    )
    .unwrap();
    let restored = load(&file);
    assert_eq!(restored.active_seconds, 311);
    assert_eq!(
        restored.previous,
        Some((5, "adjust: verify the release".into()))
    );
    save(
        &file,
        &SavedReview {
            active_seconds: 42,
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
