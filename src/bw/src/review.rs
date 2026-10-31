use crate::{session::environment_key, settings::Settings};
use blackwall_core::{
    model::HttpModel,
    policy::Policy,
    review::{ReviewSnapshot, ReviewTarget},
};
use std::path::Path;

pub async fn run(
    directory: &Path,
    path: &Path,
    settings: &Settings,
    arguments: &[String],
) -> Result<(), String> {
    let mut arguments = arguments.to_vec();
    let json = if let Some(index) = arguments.iter().position(|arg| arg == "--json") {
        arguments.remove(index);
        true
    } else {
        false
    };
    let target = ReviewTarget::parse(&arguments).map_err(|e| e.to_string())?;
    tokio::select! {
        result = execute(directory, path, settings, &target, json) => result,
        _ = tokio::signal::ctrl_c() => Err("Review cancelled; no changes were made.".into()),
    }
}

async fn execute(
    directory: &Path,
    path: &Path,
    settings: &Settings,
    target: &ReviewTarget,
    json: bool,
) -> Result<(), String> {
    // The owner's and the project's deny `file` rules keep files out of the review.
    let policy = Policy::load(directory, path).await;
    let snapshot = ReviewSnapshot::collect(path, target, Some(&policy))
        .await
        .map_err(|e| e.to_string())?;
    let backend = HttpModel::new(
        &settings.endpoint,
        &settings.model,
        environment_key(&settings.endpoint),
    )
    .and_then(|model| model.with_budget(settings.budget))
    .map_err(|e| e.to_string())?;
    let result = snapshot.review(&backend).await.map_err(|e| e.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::json!({"target": snapshot.description, "findings":result.findings, "hypotheses":result.hypotheses, "omittedUntrackedFiles":snapshot.omitted})
        );
    } else {
        println!("{}", snapshot.render(&result));
    }
    Ok(())
}
