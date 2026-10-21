//! Server events, fanned out to SSE clients (`/api/events`). Senders never
//! block; a client that falls behind misses events and is told so.

use serde::Serialize;
use tokio::sync::broadcast;
use utoipa::ToSchema;

use crate::{
    model::{Task, TaskProgress},
    monitor::MonitorStatus,
    space::SpaceStatus,
};

const CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    TaskCreated {
        task: Box<Task>,
    },
    /// State or result changed.
    TaskUpdated {
        task: Box<Task>,
    },
    TaskDeleted {
        id: i64,
    },
    /// Encode progress, at most once a second per task.
    TaskProgress(TaskProgress),
    /// Settings, profiles or monitored directories changed.
    ConfigChanged {
        section: String,
    },
    MonitorStatus {
        status: MonitorStatus,
    },
    /// Which queued transcodes wait for free space changed.
    SpaceStatus {
        status: SpaceStatus,
    },
}

impl Event {
    /// SSE event name.
    pub fn name(&self) -> &'static str {
        match self {
            Event::TaskCreated { .. } => "task_created",
            Event::TaskUpdated { .. } => "task_updated",
            Event::TaskDeleted { .. } => "task_deleted",
            Event::TaskProgress(_) => "task_progress",
            Event::ConfigChanged { .. } => "config_changed",
            Event::MonitorStatus { .. } => "monitor_status",
            Event::SpaceStatus { .. } => "space_status",
        }
    }
}

#[derive(Clone)]
pub struct Events {
    tx: broadcast::Sender<Event>,
}

impl Default for Events {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(CAPACITY).0,
        }
    }
}

impl Events {
    pub fn send(&self, event: Event) {
        // No subscribers is fine.
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn task_created(&self, task: &Task) {
        self.send(Event::TaskCreated {
            task: Box::new(task.clone()),
        });
    }

    pub fn task_updated(&self, task: &Task) {
        self.send(Event::TaskUpdated {
            task: Box::new(task.clone()),
        });
    }

    pub fn config_changed(&self, section: &str) {
        self.send(Event::ConfigChanged {
            section: section.into(),
        });
    }
}
