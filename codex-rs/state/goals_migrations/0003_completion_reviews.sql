CREATE TABLE thread_goal_completion_reviews (
    thread_id TEXT NOT NULL,
    goal_id TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    repeated_reason_count INTEGER NOT NULL DEFAULT 0,
    decision TEXT NOT NULL,
    reason TEXT NOT NULL,
    reviewed_at_ms INTEGER NOT NULL,
    PRIMARY KEY (thread_id, goal_id)
);
