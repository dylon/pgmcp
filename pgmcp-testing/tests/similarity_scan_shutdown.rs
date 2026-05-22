//! Regression test for the similarity-scan SIGTERM cleanup.
//!
//! When `batch_find_cross_project_neighbors` returns a terminal DB error
//! (e.g. `sqlx::Error::PoolClosed` after SIGTERM closes the pool mid-scan),
//! `run_similarity_scan` should:
//!
//! 1. Classify the error as `CronAction::AbortRun`,
//! 2. Emit a single INFO log line (not WARN/ERROR),
//! 3. Break the loop cleanly.
//!
//! Prior to the fix, this path emitted a `warn!` line per scan, which
//! polluted the operator log on every shutdown. The new behaviour treats
//! shutdown-time termination as expected, not anomalous.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pgmcp::config::CronConfig;
use pgmcp::cron::similarity::run_similarity_scan;
use pgmcp::daemon_state::DaemonLifecycle;
use pgmcp::db::DbClient;
use pgmcp::stats::tracker::StatsTracker;
use pgmcp_testing::mocks::MockDbClient;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

#[derive(Default)]
struct LevelCounters {
    warn: AtomicUsize,
    error: AtomicUsize,
    info: AtomicUsize,
    info_messages: parking_lot::Mutex<Vec<String>>,
}

struct CountingLayer(Arc<LevelCounters>);

impl<S: Subscriber> Layer<S> for CountingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        match *event.metadata().level() {
            Level::WARN => {
                self.0.warn.fetch_add(1, Ordering::SeqCst);
            }
            Level::ERROR => {
                self.0.error.fetch_add(1, Ordering::SeqCst);
            }
            Level::INFO => {
                self.0.info.fetch_add(1, Ordering::SeqCst);
                let mut visitor = MessageVisitor(String::new());
                event.record(&mut visitor);
                if !visitor.0.is_empty() {
                    self.0.info_messages.lock().push(visitor.0);
                }
            }
            _ => {}
        }
    }
}

struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{:?}", value);
        }
    }
}

#[tokio::test]
async fn similarity_scan_shutdown_via_pool_closed_emits_no_warnings() {
    let counters = Arc::new(LevelCounters::default());
    let layer = CountingLayer(counters.clone());
    let subscriber = Registry::default().with(layer);
    let _guard = subscriber.set_default();

    let mut mock = MockDbClient::new();
    mock.max_chunk_id_result = 10_000;
    // Allow the first two `batch_find_cross_project_neighbors` calls to
    // succeed (returning empty results so the loop advances by batch_size),
    // then fail the third with PoolClosed — simulating a SIGTERM that
    // closes the pool mid-scan after some batches have already run.
    mock.batch_neighbors_pool_closed_after = Some(2);

    let db: Arc<dyn DbClient> = Arc::new(mock);
    let stats = Arc::new(StatsTracker::new());
    let cron_cfg = CronConfig::default();
    let lifecycle = DaemonLifecycle::new();

    run_similarity_scan(db.as_ref(), &cron_cfg, 100, &stats, &lifecycle).await;

    let warns = counters.warn.load(Ordering::SeqCst);
    let errors = counters.error.load(Ordering::SeqCst);
    let infos = counters.info.load(Ordering::SeqCst);

    assert_eq!(
        warns, 0,
        "shutdown via PoolClosed must not emit warn lines, got {warns}",
    );
    assert_eq!(
        errors, 0,
        "shutdown via PoolClosed must not emit error lines, got {errors}",
    );
    assert!(
        infos >= 1,
        "shutdown via PoolClosed must emit at least one info line (the 'exiting cleanly' acknowledgement), got {infos}",
    );

    let info_msgs = counters.info_messages.lock().clone();
    let has_shutdown_ack = info_msgs.iter().any(|m| m.contains("shutdown detected"));
    assert!(
        has_shutdown_ack,
        "info lines must include the 'shutdown detected' acknowledgement; got messages {info_msgs:?}",
    );
}
