//! Explicit offline operator entry point. No listener or startup migration.
use super::*;
use orbisync_server::{checkpoint_generation::GenerationServices, checkpoint_operator};
use std::io::Read;

#[derive(Debug, Subcommand)]
pub enum OperatorCommand {
    /// Retain both fixed sources and a review template; never chooses state.
    Inspect {
        #[arg(long)]
        instance: InstanceId,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, required = true)]
        old_writer_exit_confirmed: bool,
    },
    /// Print the digest to explicitly approve after editing and reviewing input.
    ReviewDigest {
        #[arg(long)]
        review: PathBuf,
    },
    /// Approve reviewed input with separate credentials, convert, retain report.
    Apply {
        #[arg(long)]
        review: PathBuf,
        #[arg(long)]
        approve: String,
        #[arg(long)]
        report: PathBuf,
        /// Explicit retries of the SAME retained attempt in this process (0..16).
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=16))]
        retries: u8,
        #[arg(long, required = true)]
        old_writer_exit_confirmed: bool,
    },
}
fn refused() -> orbisync_application::ApplicationError {
    orbisync_application::ApplicationError::port_failure(
        "operator input, credential, or retained output unavailable",
    )
}
fn read_review(path: &Path) -> Result<serde_json::Value, orbisync_application::ApplicationError> {
    // Both preserved legacy sources plus chosen input are separately bounded.
    const MAX: u64 = 64 * 1024 * 1024;
    let file = std::fs::File::open(path).map_err(|_| refused())?;
    if file.metadata().map_err(|_| refused())?.len() > MAX {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    file.take(MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| refused())?;
    if bytes.len() as u64 > MAX {
        return Err(refused());
    }
    serde_json::from_slice(&bytes).map_err(|_| refused())
}
async fn record(
    file: &Arc<std::fs::File>,
    value: serde_json::Value,
) -> Result<(), orbisync_application::ApplicationError> {
    let file = Arc::clone(file);
    let result = tokio::task::spawn_blocking(move || {
        let mut file = &*file;
        serde_json::to_writer(&mut file, &value).map_err(|_| refused())?;
        file.write_all(b"\n").map_err(|_| refused())?;
        file.sync_all().map_err(|_| refused())
    })
    .await
    .map_err(|_| refused())?;
    if result.is_err() {
        tracing::error!(
            event = "operator.report_unknown",
            "report write/sync failed; outcome remains unknown and sources must be preserved"
        );
    }
    result
}

pub async fn run(
    action: &OperatorCommand,
    config: &Config,
    env: &SystemEnv,
) -> Result<(), ServerError> {
    if let OperatorCommand::ReviewDigest { review } = action {
        let review = review.clone();
        let digest = tokio::task::spawn_blocking(move || {
            let value = read_review(&review)?;
            Ok::<_, orbisync_application::ApplicationError>(checkpoint_operator::digest(
                &serde_json::to_vec(&value).map_err(|_| refused())?,
            ))
        })
        .await
        .map_err(|_| refused())??;
        println!("{digest}");
        return Ok(());
    }
    if config.world.checkpoint_writer_lock.trim().is_empty()
        || config.world.checkpoint_deployment.trim().is_empty()
    {
        return Err(refused().into());
    }
    let path = match action {
        OperatorCommand::Inspect { output, .. } => output,
        OperatorCommand::Apply { report, .. } => report,
        _ => return Err(refused().into()),
    };
    let path = path.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|_| refused())
    })
    .await
    .map_err(|_| refused())??;
    let output = Arc::new(output);
    let mut reviewed = if let OperatorCommand::Apply {
        review, approve, ..
    } = action
    {
        let review = review.clone();
        let artifact = tokio::task::spawn_blocking(move || read_review(&review))
            .await
            .map_err(|_| refused())??;
        let approve = approve.clone();
        let (artifact, report) = tokio::task::spawn_blocking(move || {
            let report = serde_json::json!({"status":"reviewed_input", "approved_digest":approve, "artifact":artifact,
                "outcome":"unknown_until_synced_terminal_report", "setup_and_file_sync":"separately_owned_not_in_apply_budget"});
            (artifact, report)
        }).await.map_err(|_| refused())?;
        record(&output, report).await?;
        Some(artifact)
    } else {
        None
    };
    // URL values are read exclusively from environment, never CLI or reports.
    let url = env.get(&config.database.url_env).ok_or_else(refused)?;
    let pool = create_pool(&config.database, &url)?;
    let owner = orbisync_storage_postgres::PgWriterOwnership::acquire(
        &url,
        Path::new(&config.world.checkpoint_writer_lock),
        &config.world.checkpoint_deployment,
    )
    .await?;
    let limits = orbisync_server::checkpoint_admission::checkpoint_limits(config)?;
    let store = Arc::new(orbisync_storage_postgres::PgGenerationStore::new(
        pool, limits, &owner,
    ));
    let service = GenerationServices::new(limits, owner.permit(), store)?;
    let result = async {
        match action {
            OperatorCommand::Inspect { instance, .. } => {
                let artifact = checkpoint_operator::inspect(&service, *instance).await?;
                record(&output, artifact).await?;
                Ok(())
            }
            OperatorCommand::Apply { approve, retries, .. } => {
                let artifact = reviewed.take().ok_or_else(refused)?;
                let operator_url = env.get("ORBISYNC_OPERATOR_DATABASE_URL").ok_or_else(refused)?;
                let operator = Arc::new(orbisync_storage_postgres::PgReconciliationOperator::new(create_pool(&config.database, &operator_url).map_err(|_| refused())?));
                let mut result = checkpoint_operator::apply(&service, operator, artifact, approve, SystemClock::new().now().to_unix_millis().map_err(|_| refused())?).await;
                record(&output, serde_json::json!({"status":if result.is_ok() {"committed"} else {"refused_or_uncertain"}, "attempt": service.last_conversion_identity(), "stage":service.operator_status(), "setup_and_file_sync":"separately_owned_not_in_apply_budget"})).await?;
                for _ in 0..*retries {
                    if result.is_ok() || service.pending_conversion_identity().is_none() { break; }
                    result = service.retry_conversion().await;
                    record(&output, serde_json::json!({"status":if result.is_ok() {"committed"} else {"refused_or_uncertain"}, "attempt": service.last_conversion_identity(), "stage":service.operator_status(), "setup_and_file_sync":"separately_owned_not_in_apply_budget"})).await?;
                }
                result
            }
            _ => Err(refused()),
        }
    }.await;
    service.drain().await;
    owner.shutdown().await;
    result.map_err(ServerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fix74_operator_parser_requires_explicit_inputs_and_has_no_credential_argument() {
        assert!(
            Cli::try_parse_from([
                "server",
                "checkpoint-operator",
                "apply",
                "--review",
                "review.json",
                "--approve",
                "digest",
                "--report",
                "report.jsonl",
                "--old-writer-exit-confirmed",
                "--retries",
                "2"
            ])
            .is_ok()
        );
        for args in [
            vec!["server", "checkpoint-operator", "apply"],
            vec![
                "server",
                "checkpoint-operator",
                "inspect",
                "--instance",
                "invalid",
                "--output",
                "out",
            ],
            vec![
                "server",
                "checkpoint-operator",
                "review-digest",
                "--review",
                "review.json",
                "--database-url",
                "secret",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}
