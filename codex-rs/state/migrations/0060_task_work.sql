CREATE TABLE task_work (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    summary TEXT NOT NULL,
    status TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    resource_count INTEGER NOT NULL DEFAULT 0,
    automatic_name TEXT,
    title_locked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE task_work_claims (
    resource TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES task_work(thread_id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL
);
CREATE INDEX task_work_claims_thread ON task_work_claims(thread_id);
