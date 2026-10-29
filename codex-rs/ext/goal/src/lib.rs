//! Extension crate for the `/goal` feature.

mod accounting;
mod analytics;
mod api;
mod completion_review;
mod events;
mod extension;
mod metrics;
mod progress_checkpoint;
mod progress_evidence;
mod progress_persistence;
mod progress_review;
mod runtime;
mod spec;
mod steering;
mod tool;

pub use api::GoalObjectiveUpdate;
pub use api::GoalService;
pub use api::GoalServiceError;
pub use api::GoalSetOutcome;
pub use api::GoalSetRequest;
pub use api::GoalTokenBudgetUpdate;
pub use extension::GoalExtension;
pub use extension::GoalExtensionConfig;
pub use extension::install_with_backend;
pub use progress_review::install_progress_review;
pub use runtime::GoalRuntimeHandle;
pub use runtime::PreviousGoalSnapshot;
pub use spec::CREATE_GOAL_TOOL_NAME;
pub use spec::GET_GOAL_TOOL_NAME;
pub use spec::UPDATE_GOAL_TOOL_NAME;
pub use tool::CreateGoalRequest;
