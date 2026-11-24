//! Every write runs as a task: planned → running → pending commit → committed,
//! or abandoned before it commits. Kept in memory.

use std::sync::Mutex;

use crate::proto::{SessionRef, Task, TaskState};
use crate::timefmt::now_ms;

pub struct Registry {
    tasks: Mutex<Vec<Task>>,
}

/// Whether a task may move from `from` to `to`.
pub fn allowed(from: TaskState, to: TaskState) -> bool {
    use TaskState::*;
    matches!(
        (from, to),
        (Planned, Running)
            | (Running, PendingCommit)
            | (PendingCommit, Committed)
            | (Planned | Running | PendingCommit, Abandoned)
    )
}

pub fn is_active(task: &Task) -> bool {
    matches!(
        task.state(),
        TaskState::Planned | TaskState::Running | TaskState::PendingCommit
    )
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub fn create(&self, kind: &str, sessions: Vec<SessionRef>, summary: String) -> Task {
        let now = now_ms();
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.to_string(),
            state: TaskState::Planned as i32,
            sessions,
            summary,
            created_at_ms: now,
            updated_at_ms: now,
            ..Default::default()
        };
        self.lock().push(task.clone());
        task
    }

    pub fn advance(&self, id: &str, to: TaskState, error: Option<String>) -> Result<Task, String> {
        let mut tasks = self.lock();
        let task = tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("没有任务 {id}"))?;
        if !allowed(task.state(), to) {
            return Err(format!("任务 {id} 不能从 {:?} 变成 {to:?}", task.state()));
        }
        task.set_state(to);
        task.updated_at_ms = now_ms();
        if let Some(error) = error {
            task.error = error;
        }
        Ok(task.clone())
    }

    /// Changes a task in place (progress, summary, result); the state goes through `advance`.
    pub fn update(&self, id: &str, change: impl FnOnce(&mut Task)) {
        if let Some(task) = self.lock().iter_mut().find(|t| t.id == id) {
            change(task);
            task.updated_at_ms = now_ms();
        }
    }

    pub fn get(&self, id: &str) -> Option<Task> {
        self.lock().iter().find(|t| t.id == id).cloned()
    }

    pub fn list(&self, active_only: bool, limit: usize) -> Vec<Task> {
        self.lock()
            .iter()
            .rev()
            .filter(|t| !active_only || is_active(t))
            .take(limit)
            .cloned()
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Task>> {
        self.tasks.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_state_machine() {
        let registry = Registry::new();
        let task = registry.create("archive", Vec::new(), String::new());
        assert!(registry.advance(&task.id, TaskState::Committed, None).is_err());
        registry.advance(&task.id, TaskState::Running, None).unwrap();
        registry.advance(&task.id, TaskState::PendingCommit, None).unwrap();
        let done = registry.advance(&task.id, TaskState::Committed, None).unwrap();
        assert_eq!(done.state(), TaskState::Committed);
        assert!(registry.advance(&task.id, TaskState::Abandoned, None).is_err());
        assert!(registry.list(true, 10).is_empty());
        assert_eq!(registry.list(false, 10).len(), 1);
    }
}
