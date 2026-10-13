//! Refresh jobs: what a `POST /refresh` started, and what it has done.
//!
//! A refresh is not a request. It fetches from the network, so it can outlast any timeout a proxy or
//! a browser would tolerate, and the user should be able to close the tab and come back to the
//! answer. So the handler registers a job, spawns the run and replies `202` with the job's id; the
//! client polls. This module is the registry those two ends meet at.
//!
//! Jobs live in memory only. A restart loses the record of what the previous process was doing, and
//! that is fine — the feeds themselves record `lastUpdate` and `error`, so the durable answer to
//! "did it work" is in the database and in [`crate::service::problems`], not here.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

use crate::error::Result;
use crate::service::refresh::run::Report;

/// How many finished jobs are kept before the oldest are dropped.
///
/// A bound rather than a time-to-live, because the thing worth avoiding is a long-running process
/// accumulating one record per refresh button press forever.
const KEEP: usize = 32;

/// Where a job is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    /// The run finished. Individual feeds may still have failed; those are in [`Report::errors`].
    Done,
    /// The run itself could not be carried out — the due-feed selection failed, or a task panicked.
    Failed,
}

/// One registered refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    /// Who asked. A job is only ever reported back to its owner, or to an administrator.
    pub id_user: i64,
    pub state: JobState,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// Empty while the job runs, and filled in whole when it ends.
    pub report: Report,
}

/// What [`Jobs::begin`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begin {
    /// A new job was registered; the caller owns running it and must call [`Jobs::finish`].
    New(Job),
    /// This user already has a job running, so no new one was started. The id is the running job's.
    ///
    /// Returning the existing job rather than an error is deliberate: a second press of the refresh
    /// button wants the same answer as the first, and a `409` would make the client handle a case
    /// whose correct handling is "keep waiting for the job you already have".
    Running(String),
}

/// The registry. Cheap to clone (the map is behind an `Arc`-free `Mutex`, and `Jobs` is held in an
/// `Arc` by the app state).
#[derive(Debug, Default)]
pub struct Jobs {
    inner: Mutex<HashMap<String, Job>>,
}

impl Jobs {
    /// Registers a job for `id_user`, or reports the one already running for them.
    ///
    /// One job per user rather than one globally: a refresh is that user's subscriptions and nobody
    /// else's, so two users pressing the button at the same time are two independent runs.
    pub fn begin(&self, id_user: i64, now: i64) -> Begin {
        let mut map = self.lock();
        if let Some(running) = map
            .values()
            .find(|job| job.id_user == id_user && job.state == JobState::Running)
        {
            return Begin::Running(running.id.clone());
        }
        let job = Job {
            id: uuid::Uuid::new_v4().simple().to_string(),
            id_user,
            state: JobState::Running,
            started_at: now,
            finished_at: None,
            report: Report::default(),
        };
        let begin = Begin::New(job.clone());
        map.insert(job.id.clone(), job);
        prune(&mut map);
        begin
    }

    /// Records how a run ended. A job id that is not registered is ignored rather than an error: the
    /// registry is a report, not a ledger, and the run has already happened either way.
    pub fn finish(&self, id: &str, outcome: Result<Report>, now: i64) {
        let mut map = self.lock();
        if let Some(job) = map.get_mut(id) {
            match outcome {
                Ok(report) => {
                    job.state = JobState::Done;
                    job.report = report;
                }
                Err(err) => {
                    job.state = JobState::Failed;
                    job.report
                        .errors
                        .push(crate::service::refresh::run::FeedFailure {
                            feed_id: 0,
                            name: String::new(),
                            message: err.to_string(),
                            http_status: None,
                        });
                }
            }
            job.finished_at = Some(now);
        }
    }

    /// A job by id, whatever its state.
    pub fn get(&self, id: &str) -> Option<Job> {
        self.lock().get(id).cloned()
    }

    /// The job a `GET /refresh/status` should describe: the running one if there is one, else the
    /// most recent. `None` when this user has never asked for a refresh.
    pub fn latest_for(&self, id_user: i64) -> Option<Job> {
        self.lock()
            .values()
            .filter(|job| job.id_user == id_user)
            .max_by_key(|job| (job.state == JobState::Running, job.started_at))
            .cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Job>> {
        // A poisoned lock means a panic while the map was borrowed, and the map is still consistent
        // — every method here either reads or replaces whole values. Carrying that panic into an
        // unrelated later request would be worse than continuing.
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }
}

/// Drops the oldest finished jobs once the registry is over [`KEEP`].
///
/// Running jobs are never dropped, however many there are: losing the record of a run in flight
/// would leave its client polling an id that no longer resolves.
fn prune(map: &mut HashMap<String, Job>) {
    if map.len() <= KEEP {
        return;
    }
    let mut finished: Vec<(i64, String)> = map
        .values()
        .filter(|job| job.state != JobState::Running)
        .map(|job| (job.started_at, job.id.clone()))
        .collect();
    finished.sort_unstable();
    for (_, id) in finished.iter().take(map.len() - KEEP) {
        map.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use crate::service::refresh::run::FeedFailure;

    use super::*;

    const NOW: i64 = 1_791_000_000;

    fn ok_job(jobs: &Jobs, id_user: i64, now: i64) -> Job {
        match jobs.begin(id_user, now) {
            Begin::New(job) => job,
            Begin::Running(id) => panic!("expected a new job, got a running one: {id}"),
        }
    }

    #[test]
    fn a_second_request_while_one_runs_returns_the_same_job() {
        let jobs = Jobs::default();
        let first = ok_job(&jobs, 1, NOW);
        assert_eq!(jobs.begin(1, NOW + 1), Begin::Running(first.id.clone()));
        assert_eq!(
            jobs.get(&first.id).expect("registered").state,
            JobState::Running
        );

        jobs.finish(&first.id, Ok(Report::default()), NOW + 2);
        let second = ok_job(&jobs, 1, NOW + 3);
        assert_ne!(
            second.id, first.id,
            "a finished job does not block a new one"
        );
    }

    #[test]
    fn two_users_do_not_block_each_other() {
        let jobs = Jobs::default();
        ok_job(&jobs, 1, NOW);
        let other = ok_job(&jobs, 2, NOW);
        assert_eq!(other.id_user, 2);
    }

    #[test]
    fn finishing_records_the_report() {
        let jobs = Jobs::default();
        let job = ok_job(&jobs, 1, NOW);
        let report = Report {
            feeds_done: 3,
            new_entries: 12,
            updated_entries: 1,
            muted: 0,
            errors: vec![FeedFailure {
                feed_id: 7,
                name: "Broken".to_owned(),
                message: "http 503: unavailable".to_owned(),
                http_status: Some(503),
            }],
        };
        jobs.finish(&job.id, Ok(report.clone()), NOW + 5);

        let after = jobs.get(&job.id).expect("still registered");
        assert_eq!(after.state, JobState::Done);
        assert_eq!(after.report, report);
        assert_eq!(after.finished_at, Some(NOW + 5));
    }

    #[test]
    fn a_failed_run_is_reported_rather_than_lost() {
        let jobs = Jobs::default();
        let job = ok_job(&jobs, 1, NOW);
        jobs.finish(
            &job.id,
            Err(Error::Internal("the selection query failed".to_owned())),
            NOW + 1,
        );
        let after = jobs.get(&job.id).expect("still registered");
        assert_eq!(after.state, JobState::Failed);
        assert_eq!(after.report.errors.len(), 1);
        assert!(after.report.errors[0].message.contains("selection query"));
    }

    #[test]
    fn finishing_an_unknown_job_is_ignored() {
        let jobs = Jobs::default();
        jobs.finish("nope", Ok(Report::default()), NOW);
        assert!(jobs.get("nope").is_none());
    }

    #[test]
    fn status_prefers_the_running_job_over_a_newer_finished_one() {
        let jobs = Jobs::default();
        let done = ok_job(&jobs, 1, NOW);
        jobs.finish(&done.id, Ok(Report::default()), NOW);
        let running = ok_job(&jobs, 1, NOW - 10);
        // `started_at` is older, which is why the state has to break the tie: "what is happening
        // now" is the question the spinner asks.
        assert_eq!(jobs.latest_for(1).expect("a job").id, running.id);
        assert!(jobs.latest_for(2).is_none(), "jobs are per user");
    }

    #[test]
    fn the_registry_is_bounded_but_never_drops_a_running_job() {
        let jobs = Jobs::default();
        let running = ok_job(&jobs, 1, NOW);
        for n in 1..=KEEP * 2 {
            let job = ok_job(&jobs, 2, NOW + n as i64);
            jobs.finish(&job.id, Ok(Report::default()), NOW + n as i64);
        }
        assert!(
            jobs.get(&running.id).is_some(),
            "the in-flight job survived"
        );
        assert!(
            jobs.lock().len() <= KEEP,
            "bounded: {} jobs kept",
            jobs.lock().len()
        );
        assert!(
            jobs.get(&ok_job(&jobs, 3, NOW).id).is_some(),
            "and a new one can still be registered"
        );
    }
}
