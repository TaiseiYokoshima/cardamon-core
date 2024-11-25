use super::finalise_run;
use crate::{
    entities::{iteration, run},
    execution_plan::ProcessToObserve,
    metrics_logger,
    process_control::shutdown_processes,
};
use chrono::Utc;
use sea_orm::*;
use std::{error::Error, process::exit};

pub async fn run_live<'a>(
    cpu_id: i32,
    region: Option<String>,
    ci: f64,
    processes_to_observe: Vec<ProcessToObserve>,
    db: DatabaseConnection,
) -> anyhow::Result<()> {
    let start_time = Utc::now().timestamp_millis();

    // create a new run
    let active_run = run::ActiveModel {
        id: ActiveValue::NotSet,
        is_live: ActiveValue::Set(true),
        cpu_id: ActiveValue::Set(cpu_id),
        region: ActiveValue::Set(region.clone()),
        carbon_intensity: ActiveValue::Set(ci),
        start_time: ActiveValue::Set(start_time),
        stop_time: ActiveValue::set(None),
    }
    .save(&db)
    .await?;

    // get the new run id
    let run_id = active_run.clone().try_into_model()?.id;

    // create a single iteration
    let start = Utc::now().timestamp_millis();
    let iteration = iteration::ActiveModel {
        id: ActiveValue::NotSet,
        run_id: ActiveValue::Set(run_id),
        scenario_name: ActiveValue::Set("live".to_string()),
        count: ActiveValue::Set(1),
        start_time: ActiveValue::Set(start),
        stop_time: ActiveValue::Set(None),
    };
    iteration.save(&db).await?;

    // gracefully handle ctrl-c
    let db_clone = db.clone();
    let processes = processes_to_observe.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl-C");

        shutdown_processes(&processes).expect("To shutdown processes");

        // finalise run
        let stop_time = Utc::now().timestamp_millis();
        finalise_run(run_id, stop_time, &db_clone).await.expect("");

        exit(0);
    });

    // start the metrics logger
    let mut stop_handle =
        metrics_logger::start_logging(processes_to_observe.clone(), run_id.clone(), db.clone())?;

    // keep alive!
    while let Some(_) = stop_handle.join_set.join_next().await {}

    Ok(())
}
