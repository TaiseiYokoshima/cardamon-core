use crate::{
    data::{dataset::LiveDataFilter, dataset_builder::DatasetBuilder},
    entities,
    execution_modes::finalise_run,
    execution_plan::ProcessToObserve,
    metrics_logger::{self, StopHandle},
    models::rab_model,
    process_control::shutdown_processes,
    server::{
        errors::ServerError,
        routes::{ProcessResponse, RunResponse},
    },
};
use anyhow::Context;
use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use chrono::Utc;
use colored::Colorize;
use http::{HeaderValue, Method};
use itertools::*;
use sea_orm::*;
use serde::Deserialize;
use std::{error::Error, process::exit, sync::Arc};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartParams {
    scenario_name: String,
    run_id: Option<i32>,
}

enum RunState {
    WAITING,
    RUNNING,
}

#[derive(Clone)]
struct DaemonState {
    pub is_composer: bool,
    pub cpu_id: i32,
    pub region: Option<String>,
    pub ci: f64,
    pub processes_to_observe: Vec<ProcessToObserve>,
    pub db: DatabaseConnection,

    // mutable state
    pub scenario_name: Arc<Mutex<String>>,
    pub run_id: Arc<Mutex<i32>>,
    pub run_state: Arc<Mutex<RunState>>,
    pub stop_handle: Arc<Mutex<StopHandle>>,
}

async fn create_new_run(
    cpu_id: i32,
    region: Option<String>,
    ci: f64,
    start_time: i64,
    db: &DatabaseConnection,
) -> anyhow::Result<entities::run::ActiveModel> {
    entities::run::ActiveModel {
        id: ActiveValue::NotSet,
        is_live: ActiveValue::Set(true),
        cpu_id: ActiveValue::Set(cpu_id),
        region: ActiveValue::Set(region.clone()),
        carbon_intensity: ActiveValue::Set(ci),
        start_time: ActiveValue::Set(start_time),
        stop_time: ActiveValue::set(None),
    }
    .save(db)
    .await
    .map_err(anyhow::Error::from)
}

async fn create_new_iteration(
    run_id: i32,
    scenario_name: String,
    start_time: i64,
    db: &DatabaseConnection,
) -> anyhow::Result<entities::iteration::ActiveModel> {
    entities::iteration::ActiveModel {
        id: ActiveValue::NotSet,
        run_id: ActiveValue::Set(run_id),
        scenario_name: ActiveValue::Set(scenario_name),
        count: ActiveValue::Set(1),
        start_time: ActiveValue::Set(start_time),
        stop_time: ActiveValue::Set(None), // same as start for now, will be updated later
    }
    .save(db)
    .await
    .map_err(anyhow::Error::from)
}

async fn start(
    State(state): State<DaemonState>,
    Query(params): Query<StartParams>,
) -> Result<(), ServerError> {
    println!("> start signal received");

    // check running state
    let run_state_clone = Arc::clone(&state.run_state);
    let mut run_state_mut = run_state_clone.lock().await;
    match *run_state_mut {
        RunState::WAITING => {
            // store the "scenario name" in global state
            let scenario_name_clone = Arc::clone(&state.scenario_name);
            let mut scenario_name_mut = scenario_name_clone.lock().await;
            *scenario_name_mut = params.scenario_name.clone();

            // get the "run id"
            let run_id = if state.is_composer {
                // create a new scenario, run and iteration
                let start_time = Utc::now().timestamp_millis();
                let mut run =
                    create_new_run(state.cpu_id, state.region, state.ci, start_time, &state.db)
                        .await?;
                let run_id = run.id.take().context("")?;
                create_new_iteration(run_id, params.scenario_name, start_time, &state.db).await?;
                run_id
            } else {
                params.run_id.context(
                    "Daemon running as worker expects run_id to be provided by the composer",
                )?
            };

            // store the "run id" in global state
            let run_id_clone = Arc::clone(&state.run_id);
            let mut run_id_mut = run_id_clone.lock().await;
            *run_id_mut = run_id;

            // start metric logger
            let stop_handle = metrics_logger::start_logging(
                state.processes_to_observe.clone(),
                run_id,
                state.db.clone(),
            )?;
            println!("> collecting data for run {}", run_id);

            // update run state
            *run_state_mut = RunState::RUNNING;

            // store the "stop handle" in global state
            let stop_handle_clone = Arc::clone(&state.stop_handle);
            let mut stop_handle_mut = stop_handle_clone.lock().await;
            *stop_handle_mut = stop_handle;

            Ok(())
        }
        _ => {
            // return error
            Err(ServerError(anyhow::anyhow!("Daemon is already running!")))
        }
    }
}

async fn stop(State(state): State<DaemonState>) -> Result<Json<Option<RunResponse>>, ServerError> {
    println!("> stop signal received");

    // check running state
    let run_state_clone = Arc::clone(&state.run_state);
    let mut run_state_mut = run_state_clone.lock().await;
    match *run_state_mut {
        RunState::RUNNING => {
            // stop recording!
            let stop_handle_clone = Arc::clone(&state.stop_handle);
            let mut stop_handle_mut = stop_handle_clone.lock().await;
            stop_handle_mut.stop().await;

            // update run state
            *run_state_mut = RunState::WAITING;

            if state.is_composer {
                // build a dataset for the run
                let scenario_name_clone = Arc::clone(&state.scenario_name);
                let scenario_name = scenario_name_clone.lock().await.clone();

                let run_id_clone = Arc::clone(&state.run_id);
                let run_id = run_id_clone.lock().await;

                // update "stop time" for run and iteration
                let stop_time = Utc::now().timestamp_millis();
                finalise_run(*run_id, stop_time, &state.db).await?;

                // build dataset
                let dataset = DatasetBuilder::new()
                    .scenario(&*scenario_name)
                    .all()
                    .run(*run_id)
                    .all()
                    .build(&state.db)
                    .await?;

                let scenario_datasets = dataset.by_scenario(LiveDataFilter::IncludeLive);
                let scenario_dataset = scenario_datasets.first().context("")?;

                let run_datasets = scenario_dataset.by_run();
                let run_dataset = run_datasets.first().context("")?;

                let model_data = run_dataset.apply_model(&state.db, &rab_model).await?;
                let processes = model_data
                    .process_data
                    .iter()
                    .map(|data| ProcessResponse {
                        process_name: data.process_id.clone(),
                        pow_contrib_perc: data.pow_perc,
                        iteration_metrics: data.iteration_metrics.clone(),
                    })
                    .collect_vec();

                // let json_str = serde_json::to_string_pretty(&processes);
                // println!("processes json\n{:?}", json_str);

                let run_resp = RunResponse {
                    region: model_data.region.clone(),
                    start_time: model_data.start_time,
                    duration: model_data.duration(),
                    pow: model_data.data.pow,
                    co2: model_data.data.co2,
                    ci: model_data.ci,
                    processes,
                };

                Ok(Json(Some(run_resp)))
            } else {
                Ok(Json(None))
            }
        }

        RunState::WAITING => Err(ServerError(anyhow::anyhow!("Daemon is not running"))),
    }
}

pub async fn run_daemon(
    cpu_id: i32,
    region: Option<String>,
    ci: f64,
    processes_to_observe: Vec<ProcessToObserve>,
    db: DatabaseConnection,
) -> anyhow::Result<()> {
    // gracefully handle ctrl-c
    let processes = processes_to_observe.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl-C");

        shutdown_processes(&processes).expect("To shutdown processes");
        exit(0);
    });

    // start signal server
    let app = Router::new()
        .route("/start", get(start))
        .route("/stop", get(stop))
        .layer(
            CorsLayer::new()
                .allow_origin("*".parse::<HeaderValue>().unwrap())
                .allow_methods([Method::GET]),
        )
        .with_state(DaemonState {
            is_composer: true,
            cpu_id,
            region,
            ci,
            processes_to_observe,
            db,

            scenario_name: Arc::new(Mutex::new(String::default())),
            run_id: Arc::new(Mutex::new(i32::default())),
            run_state: Arc::new(Mutex::new(RunState::WAITING)),
            stop_handle: Arc::new(Mutex::new(StopHandle::default())),
        });

    // Start the Axum server
    println!(
        "> waiting for start/stop signals on {}",
        "http://localhost:3030".green()
    );
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", 3030))
        .await
        .unwrap();

    axum::serve(listener, app).await.unwrap();

    Ok(())
}
