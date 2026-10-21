//! Retention: pending outputs past their `pending_until` are deleted (task
//! state `expired`), recommendations stop awaiting a choice after the same
//! period, recycle entries older than `recycle_retention_days` are purged,
//! stale probe cache rows are dropped. Runs at startup and hourly.

use std::{sync::Arc, time::Duration};

use fncompress_core::recycle;
use serde::Serialize;
use tokio::sync::watch;

use crate::{app::App, db::now, model::TaskState};

const EVERY: Duration = Duration::from_secs(3600);
/// Probe cache rows unused this long are dropped.
const PROBE_CACHE_DAYS: i64 = 90;

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Sweep {
    pub expired_pending: Vec<i64>,
    pub expired_choices: Vec<i64>,
    pub purged_recycle: usize,
    pub pruned_probe_cache: usize,
}

/// One pass at time `at` (unix seconds).
pub fn sweep(app: &App, at: i64) -> Sweep {
    let mut out = Sweep::default();
    match app.db.expired_pending(at) {
        Ok(tasks) => {
            for t in tasks {
                match app.discard(t.id, TaskState::Expired) {
                    Ok(_) => out.expired_pending.push(t.id),
                    Err(e) => tracing::warn!(id = t.id, "expiring pending output failed: {e}"),
                }
            }
        }
        Err(e) => tracing::error!("listing expired pending outputs failed: {e}"),
    }
    match app.db.expired_choices(at) {
        Ok(ids) => {
            for id in ids {
                match app.discard(id, TaskState::Expired) {
                    Ok(_) => out.expired_choices.push(id),
                    Err(e) => tracing::warn!(id, "expiring a recommendation failed: {e}"),
                }
            }
        }
        Err(e) => tracing::error!("listing expired recommendations failed: {e}"),
    }

    let days = app
        .db
        .global_settings()
        .map(|g| g.recycle_retention_days)
        .unwrap_or(14);
    for dir in app.db.recycle_dirs().unwrap_or_default() {
        if !dir.exists() {
            continue;
        }
        match recycle::purge_expired(&dir, days, at.max(0) as u64) {
            Ok(purged) => out.purged_recycle += purged.len(),
            Err(e) => tracing::warn!(dir = %dir.display(), "recycle purge failed: {e}"),
        }
    }

    match app.db.prune_probe_cache(at - PROBE_CACHE_DAYS * 86_400) {
        Ok(n) => out.pruned_probe_cache = n,
        Err(e) => tracing::warn!("probe cache prune failed: {e}"),
    }
    if out != Sweep::default() {
        tracing::info!(?out, "retention sweep");
    }
    out
}

/// Sweeps now and then hourly until `stop` turns true.
pub async fn run(app: Arc<App>, mut stop: watch::Receiver<bool>) {
    let mut every = tokio::time::interval(EVERY);
    loop {
        tokio::select! {
            _ = every.tick() => {
                let app = app.clone();
                let _ = tokio::task::spawn_blocking(move || sweep(&app, now())).await;
            }
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
        }
    }
}
