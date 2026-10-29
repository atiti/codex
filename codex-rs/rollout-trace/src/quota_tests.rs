use super::TraceQuota;

#[test]
fn multiple_writers_share_a_budget_and_disabled_capture_stops() -> anyhow::Result<()> {
    let temp = tempfile::TempDir::new()?;
    let enabled = temp.path().join("enabled");
    std::fs::write(&enabled, [])?;
    let quota = TraceQuota {
        root: temp.path().to_path_buf(),
        limit: 10,
        enabled_file: Some(enabled.clone()),
    };
    drop(quota.reserve(/*bytes*/ 6)?);
    let another = TraceQuota {
        root: temp.path().to_path_buf(),
        limit: 10,
        enabled_file: None,
    };
    assert!(another.reserve(/*bytes*/ 5).is_err());
    drop(another.reserve(/*bytes*/ 4)?);
    assert!(quota.reserve(/*bytes*/ 1).is_err());
    std::fs::remove_file(enabled)?;
    assert!(quota.reserve(/*bytes*/ 0).is_err());
    Ok(())
}

#[test]
fn existing_files_count_when_budget_is_missing() -> anyhow::Result<()> {
    let temp = tempfile::TempDir::new()?;
    std::fs::write(temp.path().join("old.json"), b"123456")?;
    let quota = TraceQuota {
        root: temp.path().to_path_buf(),
        limit: 8,
        enabled_file: None,
    };
    assert!(quota.reserve(/*bytes*/ 3).is_err());
    drop(quota.reserve(/*bytes*/ 2)?);
    Ok(())
}
