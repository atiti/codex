use super::*;
use crate::runtime::test_support::test_thread_metadata;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn claims_are_atomic_durable_expiring_and_released() -> anyhow::Result<()> {
    let home = unique_temp_dir();
    let config = crate::SqliteConfig::new_for_testing(home.as_path().abs());
    let db = StateRuntime::init(config.clone(), "test".into()).await?;
    let first = ThreadId::new();
    let second = ThreadId::new();
    for id in [first, second] {
        db.upsert_thread(&test_thread_metadata(&home, id, home.clone()))
            .await?;
    }
    let work = TaskWork {
        title: "Fix SendJump proxy".into(),
        summary: "Implementing the reviewed fix".into(),
        status: TaskWorkStatus::Working,
        resources: vec!["github:markster-io/sendjump#27".into()],
    };
    let (a, b) = tokio::join!(
        db.update_task_work(first, &work),
        db.update_task_work(second, &work)
    );
    let a = a?;
    let b = b?;
    assert_eq!((a.is_empty(), b.is_empty()), (a.is_empty(), !a.is_empty()));
    let (owner, other) = if a.is_empty() {
        (first, second)
    } else {
        (second, first)
    };
    let expected = vec![TaskWorkOwner {
        thread_id: owner.to_string(),
        title: work.title.clone(),
        summary: work.summary.clone(),
        resource: work.resources[0].clone(),
    }];
    assert_eq!(db.task_work_owners(other, &work.resources).await?, expected);
    let reopened = StateRuntime::init(config, "test".into()).await?;
    assert_eq!(reopened.update_task_work(other, &work).await?, expected);
    // A multi-resource request cannot keep the non-conflicting subset.
    let mut both = work.clone();
    both.resources.push("bug:sendjump:second".into());
    assert_eq!(db.update_task_work(other, &both).await?, expected);
    assert_eq!(
        db.task_work_owners(owner, &["bug:sendjump:second".into()])
            .await?,
        Vec::new()
    );
    db.lock_task_work_title(owner).await?;
    assert_eq!(db.task_work_title(owner).await?, None);
    sqlx::query("UPDATE task_work_claims SET expires_at = unixepoch() - 1")
        .execute(db.pool.as_ref())
        .await?;
    assert!(db.heartbeat_task_work(owner).await.is_err());
    assert_eq!(reopened.update_task_work(other, &work).await?, Vec::new());
    assert!(db.heartbeat_task_work(owner).await.is_err());
    assert_eq!(db.update_task_work(owner, &work).await?.len(), 1);
    let waiting = TaskWork {
        status: TaskWorkStatus::Waiting,
        ..work.clone()
    };
    assert_eq!(db.update_task_work(other, &waiting).await?, Vec::new());
    assert_eq!(db.update_task_work(owner, &work).await?, Vec::new());
    let completed = TaskWork {
        status: TaskWorkStatus::Completed,
        ..work
    };
    assert_eq!(db.update_task_work(owner, &completed).await?, Vec::new());
    assert_eq!(db.task_work_owners(other, &[]).await?, Vec::new());
    let oversized = TaskWork {
        title: "x".repeat(97),
        ..completed
    };
    assert!(db.update_task_work(other, &oversized).await.is_err());
    let file = home.join("main.rs");
    let file_work = TaskWork {
        title: "Edit one file".into(),
        summary: String::new(),
        status: TaskWorkStatus::Working,
        resources: vec![format!("file:{}", file.display())],
    };
    assert!(db.update_task_work(owner, &file_work).await?.is_empty());
    for alias in [
        home.join("src").join("..").join("main.rs"),
        home.join(".").join("main.rs"),
    ] {
        let alias_work = TaskWork {
            resources: vec![format!("file:{}", alias.display())],
            ..file_work.clone()
        };
        assert!(db.update_task_work(other, &alias_work).await.is_err());
    }
    assert_eq!(db.update_task_work(other, &file_work).await?.len(), 1);
    drop(reopened);
    drop(db);
    std::fs::remove_dir_all(home)?;
    Ok(())
}
