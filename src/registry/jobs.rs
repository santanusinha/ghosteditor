//! Install job manager (P2.6b).
//!
//! One detached task per recipe. Idempotent: a second call while a job runs
//! returns the running job. Jobs outlive the tool call that started them,
//! so `install_server` can return "running" before the install finishes.
//! Every job writes its log to `~/.cache/ghostedit/installs/<name>.log`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::install::{self, InstallPlan};
use super::Recipe;

/// Where one job writes its log.
pub fn install_log_path(name: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".cache/ghostedit/installs")
            .join(format!("{name}.log")),
    )
}

/// The observable state of one job.
#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Running,
    Installed,
    Failed(String),
}

impl JobStatus {
    fn label(&self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Installed => "installed",
            JobStatus::Failed(_) => "failed",
        }
    }
}

struct Job {
    status: JobStatus,
    /// Kept so the task is not detached-and-forgotten; abort is not used.
    #[allow(dead_code)]
    handle: Option<JoinHandle<()>>,
}

/// The job table. Keyed by recipe name; one job per name at a time.
/// The `Server` owns one `Arc<JobManager>` for the whole process.
#[derive(Default)]
pub struct JobManager {
    jobs: Mutex<HashMap<String, Job>>,
}

impl JobManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// The status of a job, when one exists. `None` means "no job ever ran".
    pub async fn status(&self, name: &str) -> Option<JobStatus> {
        self.jobs
            .lock()
            .await
            .get(name)
            .map(|job| job.status.clone())
    }

    /// Start an install for `recipe`. Idempotent: a running or finished job
    /// for the same name is returned as is. The install runs detached; this
    /// method returns once the job is registered, not when it finishes.
    pub async fn start(
        self: &Arc<Self>,
        recipe: Arc<Recipe>,
        version: Option<String>,
    ) -> JobStatus {
        let name = recipe.name.clone();
        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get(&name) {
            return job.status.clone();
        }
        let status = JobStatus::Running;
        jobs.insert(
            name.clone(),
            Job {
                status: status.clone(),
                handle: None,
            },
        );
        drop(jobs);

        let handle = tokio::spawn(run_job(self.clone(), recipe, version));
        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get_mut(&name) {
            job.handle = Some(handle);
        }
        status
    }

    /// Replace the status when the job ends. Called by `run_job`.
    async fn finish(&self, name: &str, status: JobStatus) {
        let mut jobs = self.jobs.lock().await;
        match jobs.get_mut(name) {
            Some(job) => job.status = status,
            None => {
                jobs.insert(
                    name.to_string(),
                    Job {
                        status,
                        handle: None,
                    },
                );
            }
        }
    }
}

/// One install to completion: plan, check the toolchain, run the steps,
/// write the log, update the status. Runs detached.
async fn run_job(manager: Arc<JobManager>, recipe: Arc<Recipe>, version: Option<String>) {
    let name = recipe.name.clone();
    let mut log = String::new();
    let root = super::install_root().unwrap_or_else(|| PathBuf::from("."));
    let result = run_install(&recipe, version.as_deref(), &root, &mut log).await;
    let status = match result {
        Ok(()) => JobStatus::Installed,
        Err(reason) => JobStatus::Failed(reason),
    };
    if let Some(path) = install_log_path(&name) {
        write_log(&path, &log).await;
    }
    manager.finish(&name, status).await;
}

/// Plan and execute one install. The log records every step.
async fn run_install(
    recipe: &Recipe,
    version: Option<&str>,
    root: &std::path::Path,
    log: &mut String,
) -> Result<(), String> {
    let plan: InstallPlan = install::plan(recipe, version, root)?;
    install::check_toolchain(&plan)?;
    for step in &plan.steps {
        install::run_step(step, log).await?;
    }
    if !plan.bin_path.is_file() {
        return Err(format!(
            "install finished but {} is missing",
            plan.bin_path.display()
        ));
    }
    log.push_str(&format!("installed: {}\n", plan.bin_path.display()));
    Ok(())
}

/// Write the job log. Best effort; a log failure must not fail the job.
async fn write_log(path: &std::path::Path, log: &str) {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(path, log).await;
}

/// The public JSON payload for a job status (P2.6c).
pub fn status_payload(name: &str, status: &JobStatus) -> serde_json::Value {
    match status {
        JobStatus::Running => serde_json::json!({
            "text": format!("Install for {name} is running. Call install_server again for the result."),
            "server": name,
            "status": status.label(),
        }),
        JobStatus::Installed => serde_json::json!({
            "text": format!("Installed {name}."),
            "server": name,
            "status": status.label(),
        }),
        JobStatus::Failed(reason) => serde_json::json!({
            "text": format!("Install for {name} failed: {reason}"),
            "server": name,
            "status": status.label(),
            "reason": reason,
            "log": install_log_path(name)
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_recipe() -> Recipe {
        serde_json::from_value(serde_json::json!({
            "name": "ghostedit-test-server",
            "version": "1.0.0",
            "languages": [],
            "homepage": "",
            "supported": true,
            "source": { "kind": "npm", "package": "no-such-package-ghostedit" },
            "bin": "no-such-bin",
        }))
        .unwrap()
    }

    async fn wait_until_done(manager: &JobManager, name: &str, tries: usize) -> Option<JobStatus> {
        for _ in 0..tries {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if let Some(status) = manager.status(name).await {
                if status != JobStatus::Running {
                    return Some(status);
                }
            }
        }
        None
    }

    #[tokio::test]
    async fn failed_install_lands_in_the_table() {
        let manager = Arc::new(JobManager::new());
        let recipe = Arc::new(test_recipe());
        let status = manager.start(recipe, None).await;
        assert_eq!(status, JobStatus::Running);
        // npm must fail fast on a package that does not exist.
        let done = wait_until_done(&manager, "ghostedit-test-server", 120).await;
        let done = done.expect("job did not finish within 30 s");
        assert!(matches!(done, JobStatus::Failed(_)), "was {done:?}");
    }

    #[tokio::test]
    async fn idempotent_while_running() {
        let manager = Arc::new(JobManager::new());
        let recipe = Arc::new(test_recipe());
        let first = manager.start(recipe.clone(), None).await;
        let second = manager.start(recipe, None).await;
        assert_eq!(first, second);
        assert_eq!(second, JobStatus::Running);
        // Drain the detached task so the test runtime shuts down cleanly.
        let done = wait_until_done(&manager, "ghostedit-test-server", 120).await;
        assert!(done.is_some(), "job did not finish");
    }

    #[test]
    fn status_payload_labels() {
        assert_eq!(
            status_payload("x", &JobStatus::Running)["status"],
            "running"
        );
        assert_eq!(
            status_payload("x", &JobStatus::Installed)["status"],
            "installed"
        );
        assert_eq!(
            status_payload("x", &JobStatus::Failed("boom".into()))["status"],
            "failed"
        );
    }
}
