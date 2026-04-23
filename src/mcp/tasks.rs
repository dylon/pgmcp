//! Task store for long-running MCP operations (e.g., reindex).
//!
//! Tracks task lifecycle via `DashMap<String, TaskState>` for concurrent access.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use dashmap::DashMap;
use rmcp::model::{Task, TaskStatus};

/// Internal state for a tracked task.
pub struct TaskState {
    /// MCP task metadata.
    pub task: Task,
    /// The result payload (populated on completion).
    pub result: Option<serde_json::Value>,
    /// Flag that workers check to support cancellation.
    pub cancel_flag: Arc<AtomicBool>,
}

/// Concurrent store for active and completed tasks.
pub struct TaskStore {
    tasks: DashMap<String, TaskState>,
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskStore {
    pub fn new() -> Self {
        Self {
            tasks: DashMap::new(),
        }
    }

    /// Create a new task in `Working` status. Returns (task_id, cancel_flag).
    pub fn create_task(&self, tool_name: &str) -> (String, Arc<AtomicBool>) {
        let task_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let task = Task::new(task_id.clone(), TaskStatus::Working, now.clone(), now)
            .with_status_message(format!("{} starting...", tool_name))
            .with_poll_interval(1000);

        self.tasks.insert(
            task_id.clone(),
            TaskState {
                task,
                result: None,
                cancel_flag: Arc::clone(&cancel_flag),
            },
        );

        (task_id, cancel_flag)
    }

    /// Update the progress message for a task.
    pub fn update_progress(&self, task_id: &str, message: &str) {
        if let Some(mut entry) = self.tasks.get_mut(task_id) {
            entry.task = entry.task.clone().with_status_message(message);
            // Update the last_updated_at field
            entry.task.last_updated_at = Utc::now().to_rfc3339();
        }
    }

    /// Mark a task as completed with its result payload.
    pub fn complete_task(&self, task_id: &str, result: serde_json::Value) {
        if let Some(mut entry) = self.tasks.get_mut(task_id) {
            entry.task.status = TaskStatus::Completed;
            entry.task.last_updated_at = Utc::now().to_rfc3339();
            entry.task = entry.task.clone().with_status_message("Completed");
            entry.result = Some(result);
        }
    }

    /// Mark a task as failed with an error message.
    pub fn fail_task(&self, task_id: &str, error: &str) {
        if let Some(mut entry) = self.tasks.get_mut(task_id) {
            entry.task.status = TaskStatus::Failed;
            entry.task.last_updated_at = Utc::now().to_rfc3339();
            entry.task = entry.task.clone().with_status_message(error);
        }
    }

    /// Cancel a task. Sets the cancel flag and updates status.
    pub fn cancel_task(&self, task_id: &str) -> Option<Task> {
        if let Some(mut entry) = self.tasks.get_mut(task_id) {
            entry.cancel_flag.store(true, Ordering::Release);
            entry.task.status = TaskStatus::Cancelled;
            entry.task.last_updated_at = Utc::now().to_rfc3339();
            entry.task = entry
                .task
                .clone()
                .with_status_message("Cancelled by client");
            Some(entry.task.clone())
        } else {
            None
        }
    }

    /// Get a task's current metadata.
    pub fn get_task(&self, task_id: &str) -> Option<Task> {
        self.tasks.get(task_id).map(|e| e.task.clone())
    }

    /// Get a task's result payload.
    pub fn get_result(&self, task_id: &str) -> Option<serde_json::Value> {
        self.tasks.get(task_id).and_then(|e| e.result.clone())
    }

    /// List all tasks.
    pub fn list_tasks(&self) -> Vec<Task> {
        self.tasks.iter().map(|e| e.task.clone()).collect()
    }
}
