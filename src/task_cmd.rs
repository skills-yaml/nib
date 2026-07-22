use chrono::Utc;
use clap::{Args, Subcommand};
use std::path::Path;

#[derive(Args, Debug)]
pub struct TaskArgs {
    #[command(subcommand)]
    pub command: TaskCommand,
}

#[derive(Subcommand, Debug)]
pub enum TaskCommand {
    /// List durable background terminal and scheduled jobs
    List,
    /// Inspect one durable job
    Get { task_id: String },
    /// Request cancellation and wait briefly for worker reconciliation
    Cancel { task_id: String },
    /// Mark workers with expired leases as failed without replaying side effects
    Reconcile,
}

pub fn run_task_cmd(args: &TaskArgs, project: &Path) -> Result<(), String> {
    let store = nib::daemons::workload::DurableTaskStore::for_project(project)?;
    let value = match &args.command {
        TaskCommand::List => serde_json::json!({"tasks": store.list()?}),
        TaskCommand::Get { task_id } => serde_json::json!({
            "task": store
                .get(task_id)?
                .ok_or_else(|| format!("background task not found: {task_id}"))?
        }),
        TaskCommand::Cancel { task_id } => {
            serde_json::json!({"task": store.cancel(task_id)?})
        }
        TaskCommand::Reconcile => {
            let report = store.reconcile(Utc::now())?;
            serde_json::json!({
                "tasks": report.tasks,
                "scanned_records": report.scanned_records,
                "reconciled_records": report.reconciled_records,
                "omitted_records": report.omitted_records,
            })
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&value)
            .map_err(|error| format!("failed to encode task output: {error}"))?
    );
    Ok(())
}
