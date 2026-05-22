//! `CronState` + `CronEvent` enums — extracted from `scheduler.rs` as
//! part of the D.2 god-file split.

// ============================================================================
// CronState
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronState {
    CheckEvents,
    DrainChannel,
    ExecutingTask,
    Sleeping,
    Terminated,
}

// ============================================================================
// CronEvent
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronEvent {
    TaskReceived,
    TimerExpired,
    TaskDue,
    TaskCompleted { success: bool, should_requeue: bool },
    TerminationRequested,
    ChannelDisconnected,
    NoEvents,
}
