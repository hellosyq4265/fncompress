use std::path::{Path, PathBuf};

use fncompress_core::{
    pipeline::{Candidate, Outcome},
    recommend::Recommendation,
};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, ToSql};

use super::{from_json, from_name, name, now, path_str, to_json, Db, Result};
use crate::model::{NewTask, PendingReason, RecommendChoice, Task, TaskSource, TaskState};

const COLUMNS: &str = "id, kind, source, batch_id, monitor_id, path, state, quota, force, \
    profile_id, fingerprint, snapshot, observe, outcome, candidate, pending_reason, \
    pending_until, error, created_at, started_at, finished_at, recommend, recommendation, choice";

/// Task list query; every field narrows the result.
#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub states: Vec<TaskState>,
    pub source: Option<TaskSource>,
    pub batch_id: Option<i64>,
    /// Only tasks with a smaller id (paging, newest first).
    pub before: Option<i64>,
    pub limit: Option<u32>,
}

fn insert_task(conn: &Connection, t: &NewTask) -> Result<()> {
    conn.execute(
        "INSERT INTO tasks (kind, source, batch_id, monitor_id, path, state, quota, force,          profile_id, fingerprint, snapshot, created_at, recommend)          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            name(&t.kind),
            name(&t.source),
            t.batch_id,
            t.monitor_id,
            path_str(&t.path),
            name(&TaskState::Queued),
            name(&t.quota()),
            t.force,
            t.snapshot.profile.id,
            t.snapshot.profile.fingerprint(),
            to_json(&t.snapshot),
            now(),
            t.recommend.as_ref().map(to_json),
        ],
    )?;
    Ok(())
}

fn row_to_task(r: &Row) -> Result<Task> {
    let opt_json = |col: usize| -> Result<Option<String>> { r.get(col) };
    Ok(Task {
        id: r.get(0)?,
        kind: from_name(1, r.get(1)?)?,
        source: from_name(2, r.get(2)?)?,
        batch_id: r.get(3)?,
        monitor_id: r.get(4)?,
        path: PathBuf::from(r.get::<_, String>(5)?),
        state: from_name(6, r.get(6)?)?,
        quota: from_name(7, r.get(7)?)?,
        force: r.get(8)?,
        profile_id: r.get(9)?,
        fingerprint: r.get(10)?,
        snapshot: from_json(11, &r.get::<_, String>(11)?)?,
        observe: r.get(12)?,
        outcome: opt_json(13)?.map(|s| from_json(13, &s)).transpose()?,
        candidate: opt_json(14)?.map(|s| from_json(14, &s)).transpose()?,
        pending_reason: r
            .get::<_, Option<String>>(15)?
            .map(|s| from_name(15, s))
            .transpose()?,
        pending_until: r.get(16)?,
        error: r.get(17)?,
        created_at: r.get(18)?,
        started_at: r.get(19)?,
        finished_at: r.get(20)?,
        recommend: opt_json(21)?.map(|s| from_json(21, &s)).transpose()?,
        recommendation: opt_json(22)?.map(|s| from_json(22, &s)).transpose()?.map(
            |mut r: Recommendation| {
                r.upgrade();
                r
            },
        ),
        choice: opt_json(23)?.map(|s| from_json(23, &s)).transpose()?,
        progress: None,
    })
}

impl Db {
    pub fn create_batch(&self, dir: &Path) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO batches (dir, created_at) VALUES (?1, ?2)",
            params![path_str(dir), now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn insert_task(&self, t: &NewTask) -> Result<Task> {
        let id = {
            let conn = self.conn();
            insert_task(&conn, t)?;
            conn.last_insert_rowid()
        };
        Ok(self.task(id)?.expect("just inserted"))
    }

    /// A folder batch and its tasks in one transaction. `tasks` get the new
    /// batch id; the inserted tasks come back in id order.
    pub fn insert_batch(&self, dir: &Path, tasks: &[NewTask]) -> Result<(i64, Vec<Task>)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO batches (dir, created_at) VALUES (?1, ?2)",
            params![path_str(dir), now()],
        )?;
        let batch_id = tx.last_insert_rowid();
        for t in tasks {
            insert_task(
                &tx,
                &NewTask {
                    batch_id: Some(batch_id),
                    ..t.clone()
                },
            )?;
        }
        tx.commit()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE batch_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([batch_id], row_to_task)?;
        Ok((batch_id, rows.collect::<Result<_>>()?))
    }

    pub fn task(&self, id: i64) -> Result<Option<Task>> {
        self.conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM tasks WHERE id = ?1"),
                [id],
                row_to_task,
            )
            .optional()
    }

    /// Newest first.
    pub fn tasks(&self, f: &TaskFilter) -> Result<Vec<Task>> {
        let mut sql = format!("SELECT {COLUMNS} FROM tasks WHERE 1 = 1");
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        if !f.states.is_empty() {
            let marks = vec!["?"; f.states.len()].join(", ");
            sql.push_str(&format!(" AND state IN ({marks})"));
            args.extend(f.states.iter().map(|s| Box::new(name(s)) as Box<dyn ToSql>));
        }
        if let Some(s) = f.source {
            sql.push_str(" AND source = ?");
            args.push(Box::new(name(&s)));
        }
        if let Some(b) = f.batch_id {
            sql.push_str(" AND batch_id = ?");
            args.push(Box::new(b));
        }
        if let Some(b) = f.before {
            sql.push_str(" AND id < ?");
            args.push(Box::new(b));
        }
        sql.push_str(" ORDER BY id DESC");
        if let Some(l) = f.limit {
            sql.push_str(&format!(" LIMIT {l}"));
        }
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), row_to_task)?;
        rows.collect()
    }

    /// Queued tasks, oldest first: the dispatch order.
    pub fn queued(&self) -> Result<Vec<Task>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE state = 'queued' ORDER BY id"
        ))?;
        let rows = stmt.query_map([], row_to_task)?;
        rows.collect()
    }

    /// A queued, running or pending transcode of this path (a file is not
    /// queued twice).
    pub fn active_task_for(&self, path: &Path) -> Result<Option<i64>> {
        self.conn()
            .query_row(
                "SELECT id FROM tasks WHERE path = ?1 AND kind = 'transcode' \
                 AND state IN ('queued', 'running', 'pending') ORDER BY id DESC LIMIT 1",
                [path_str(path)],
                |r| r.get(0),
            )
            .optional()
    }

    /// A recommendation for this path that is queued, running or awaiting a
    /// choice: the monitor holds the file back meanwhile.
    pub fn recommend_hold_for(&self, path: &Path) -> Result<Option<i64>> {
        self.conn()
            .query_row(
                "SELECT id FROM tasks WHERE path = ?1 AND kind = 'recommend' \
                 AND state IN ('queued', 'running', 'awaiting_choice') ORDER BY id DESC LIMIT 1",
                [path_str(path)],
                |r| r.get(0),
            )
            .optional()
    }

    /// Measurements so far, while the recommendation runs.
    pub fn set_recommendation(&self, id: i64, rec: Option<&Recommendation>) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET recommendation = ?2 WHERE id = ?1",
            params![id, rec.map(to_json)],
        )?;
        Ok(())
    }

    /// running → awaiting_choice with the final measurements.
    pub fn await_choice(&self, id: i64, rec: &Recommendation, until: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET state = 'awaiting_choice', recommendation = ?2, pending_until = ?3, \
             finished_at = ?4 WHERE id = ?1 AND state = 'running'",
            params![id, to_json(rec), until, now()],
        )?;
        Ok(())
    }

    /// awaiting_choice → chosen. False if it was no longer awaiting.
    pub fn choose(&self, id: i64, choice: &RecommendChoice) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE tasks SET state = 'chosen', choice = ?2, pending_until = NULL \
             WHERE id = ?1 AND state = 'awaiting_choice'",
            params![id, to_json(choice)],
        )?;
        Ok(n == 1)
    }

    /// awaiting_choice → discarded/expired.
    pub fn close_choice(&self, id: i64, state: TaskState) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE tasks SET state = ?2, pending_until = NULL \
             WHERE id = ?1 AND state = 'awaiting_choice'",
            params![id, name(&state)],
        )?;
        Ok(n == 1)
    }

    /// Recommendations still awaiting a choice after `at`.
    pub fn expired_choices(&self, at: i64) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM tasks WHERE state = 'awaiting_choice' AND pending_until < ?1 \
             ORDER BY id",
        )?;
        let rows = stmt.query_map([at], |r| r.get(0))?;
        rows.collect()
    }

    /// queued → running. False if the task was no longer queued (cancelled
    /// in between).
    pub fn mark_running(&self, id: i64, observe: bool) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE tasks SET state = 'running', observe = ?2, started_at = ?3 \
             WHERE id = ?1 AND state = 'queued'",
            params![id, observe, now()],
        )?;
        Ok(n == 1)
    }

    /// running → a final state with the pipeline's result.
    pub fn finish_task(
        &self,
        id: i64,
        state: TaskState,
        outcome: Option<&Outcome>,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET state = ?2, outcome = ?3, error = ?4, finished_at = ?5 WHERE id = ?1",
            params![id, name(&state), outcome.map(to_json), error, now()],
        )?;
        Ok(())
    }

    /// running → pending, keeping the candidate for release.
    pub fn mark_pending(
        &self,
        id: i64,
        outcome: &Outcome,
        candidate: &Candidate,
        reason: PendingReason,
        until: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE tasks SET state = 'pending', outcome = ?2, candidate = ?3, pending_reason = ?4, \
             pending_until = ?5, finished_at = ?6 WHERE id = ?1",
            params![
                id,
                to_json(outcome),
                to_json(candidate),
                name(&reason),
                until,
                now()
            ],
        )?;
        Ok(())
    }

    /// pending → released/discarded/expired/insufficient_savings. The
    /// candidate is kept for the record; false if the task was not pending.
    pub fn close_pending(
        &self,
        id: i64,
        state: TaskState,
        outcome: Option<&Outcome>,
        error: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE tasks SET state = ?2, outcome = COALESCE(?3, outcome), error = ?4, \
             pending_until = NULL, finished_at = ?5 WHERE id = ?1 AND state = 'pending'",
            params![id, name(&state), outcome.map(to_json), error, now()],
        )?;
        Ok(n == 1)
    }

    /// Changes a task's state only if it is currently `from`.
    pub fn transition(&self, id: i64, from: TaskState, to: TaskState) -> Result<bool> {
        let finished = to.is_final().then(now);
        let n = self.conn().execute(
            "UPDATE tasks SET state = ?3, finished_at = COALESCE(?4, finished_at) \
             WHERE id = ?1 AND state = ?2",
            params![id, name(&from), name(&to), finished],
        )?;
        Ok(n == 1)
    }

    /// Every pending output, oldest first.
    pub fn pending_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE state = 'pending' ORDER BY id"
        ))?;
        let rows = stmt.query_map([], row_to_task)?;
        rows.collect()
    }

    /// Pending outputs whose retention ended before `at`.
    pub fn expired_pending(&self, at: i64) -> Result<Vec<Task>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE state = 'pending' AND pending_until < ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([at], row_to_task)?;
        rows.collect()
    }

    /// Tasks interrupted by a shutdown or crash go back to the queue
    /// (they restart from scratch). Returns how many.
    pub fn requeue_running(&self) -> Result<usize> {
        self.conn().execute(
            "UPDATE tasks SET state = 'queued', started_at = NULL, observe = 0 \
             WHERE state = 'running'",
            [],
        )
    }

    /// Deletes a task that is no longer active.
    pub fn delete_task(&self, id: i64) -> Result<bool> {
        let n = self.conn().execute(
            "DELETE FROM tasks WHERE id = ?1 \
             AND state NOT IN ('queued', 'running', 'pending', 'awaiting_choice')",
            [id],
        )?;
        Ok(n == 1)
    }
}

#[cfg(test)]
mod tests {
    use fncompress_core::settings::SettingsSnapshot;

    use super::*;
    use crate::model::TaskKind;

    pub(crate) fn new_task(path: &str) -> NewTask {
        NewTask {
            kind: TaskKind::Transcode,
            source: TaskSource::Manual,
            batch_id: None,
            monitor_id: None,
            path: path.into(),
            force: false,
            snapshot: SettingsSnapshot::default(),
            recommend: None,
        }
    }

    #[test]
    fn a_batch_and_its_tasks_go_in_together() {
        let db = Db::open_in_memory().unwrap();
        let other = db.insert_task(&new_task("/m/x.mkv")).unwrap();
        let (batch, tasks) = db
            .insert_batch(
                Path::new("/m"),
                &[new_task("/m/a.mkv"), new_task("/m/b.mkv")],
            )
            .unwrap();
        assert_eq!(
            tasks.iter().map(|t| t.path.clone()).collect::<Vec<_>>(),
            [PathBuf::from("/m/a.mkv"), PathBuf::from("/m/b.mkv")]
        );
        assert!(tasks.iter().all(|t| t.batch_id == Some(batch)));
        assert!(tasks[0].id > other.id && tasks[0].id < tasks[1].id);
        assert_eq!(db.task(other.id).unwrap().unwrap().batch_id, None);
    }

    #[test]
    fn insert_list_and_filter() {
        let db = Db::open_in_memory().unwrap();
        let a = db.insert_task(&new_task("/m/a.mkv")).unwrap();
        assert_eq!(a.state, TaskState::Queued);
        assert_eq!(a.profile_id, "default");
        assert_eq!(
            a.fingerprint,
            SettingsSnapshot::default().profile.fingerprint()
        );
        let batch = db.create_batch(Path::new("/m")).unwrap();
        let b = db
            .insert_task(&NewTask {
                source: TaskSource::FolderBatch,
                batch_id: Some(batch),
                ..new_task("/m/b.mkv")
            })
            .unwrap();

        let all = db.tasks(&TaskFilter::default()).unwrap();
        assert_eq!(all.iter().map(|t| t.id).collect::<Vec<_>>(), [b.id, a.id]);
        let batched = db
            .tasks(&TaskFilter {
                batch_id: Some(batch),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(batched.len(), 1);
        assert_eq!(batched[0].source, TaskSource::FolderBatch);
        let paged = db
            .tasks(&TaskFilter {
                before: Some(b.id),
                limit: Some(10),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(paged.len(), 1);
        assert_eq!(paged[0].id, a.id);

        assert_eq!(db.queued().unwrap()[0].id, a.id);
        assert!(db.mark_running(a.id, true).unwrap());
        assert!(!db.mark_running(a.id, true).unwrap());
        let running = db
            .tasks(&TaskFilter {
                states: vec![TaskState::Running],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(running.len(), 1);
        assert!(running[0].observe);
        assert_eq!(
            db.active_task_for(Path::new("/m/a.mkv")).unwrap(),
            Some(a.id)
        );
    }

    #[test]
    fn interrupted_tasks_are_requeued() {
        let db = Db::open_in_memory().unwrap();
        let a = db.insert_task(&new_task("/m/a.mkv")).unwrap();
        db.mark_running(a.id, true).unwrap();
        assert_eq!(db.requeue_running().unwrap(), 1);
        let a = db.task(a.id).unwrap().unwrap();
        assert_eq!(a.state, TaskState::Queued);
        assert!(!a.observe);
        assert_eq!(a.started_at, None);
    }

    #[test]
    fn transitions_are_conditional_and_active_tasks_stay() {
        let db = Db::open_in_memory().unwrap();
        let a = db.insert_task(&new_task("/m/a.mkv")).unwrap();
        assert!(!db.delete_task(a.id).unwrap());
        assert!(!db
            .transition(a.id, TaskState::Running, TaskState::Cancelled)
            .unwrap());
        assert!(db
            .transition(a.id, TaskState::Queued, TaskState::Cancelled)
            .unwrap());
        let t = db.task(a.id).unwrap().unwrap();
        assert_eq!(t.state, TaskState::Cancelled);
        assert!(t.finished_at.is_some());
        assert!(db.delete_task(a.id).unwrap());
        assert!(db.task(a.id).unwrap().is_none());
    }
}
