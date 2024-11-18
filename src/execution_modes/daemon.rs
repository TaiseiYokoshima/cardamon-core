use crate::{
    data::dataset::{Dataset, ScenarioRunDataset},
    entities,
    execution_plan::ProcessToObserve,
    metrics_logger,
    models::rab_model,
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
use itertools::*;
use sea_orm::*;
use serde::{self, Deserialize};
use tokio::sync::mpsc;

#[derive(Clone)]
struct ServerState {
    tx: mpsc::Sender<Signal>,
    db: DatabaseConnection,
}

enum Signal {
    Start {
        run_id: String,
        resp_tx: mpsc::Sender<Result<(), ServerError>>,
    },
    Stop {
        resp_tx: mpsc::Sender<Result<Dataset, ServerError>>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartParams {
    pub run_id: String,
}

async fn start(
    State(state): State<ServerState>,
    Query(params): Query<StartParams>,
) -> Result<(), ServerError> {
    println!("start received {}", params.run_id);
    let (resp_tx, mut resp_rx) = mpsc::channel::<Result<(), ServerError>>(1);
    state
        .tx
        .send(Signal::Start {
            run_id: params.run_id,
            resp_tx,
        })
        .await
        .context("Failed to send start signal")?;

    resp_rx
        .recv()
        .await
        .unwrap_or(Err(ServerError(anyhow::anyhow!(
            "Error receiving response to start signal"
        ))))
}

async fn stop(State(state): State<ServerState>) -> Result<Json<RunResponse>, ServerError> {
    println!("stop received");
    let (resp_tx, mut resp_rx) = mpsc::channel::<Result<Dataset, ServerError>>(1);
    state
        .tx
        .send(Signal::Stop { resp_tx })
        .await
        .context("Failed to send stop signal")?;

    let dataset = resp_rx
        .recv()
        .await
        .unwrap_or(Err(ServerError(anyhow::anyhow!(
            "Error receiving response to stop signal"
        ))))?;

    let scenario_datasets = dataset.by_scenario(crate::data::dataset::LiveDataFilter::OnlyLive);
    let scenario_dataset = scenario_datasets
        .first()
        .context("Expected at least one scenario")?;

    let scenario_run_datasets = scenario_dataset.by_run();
    let scenario_run_dataset = scenario_run_datasets
        .first()
        .context("Expected at least one run")?;

    let model_data = scenario_run_dataset
        .apply_model(&state.db, &rab_model)
        .await?;

    let processes = model_data
        .process_data
        .iter()
        .map(|data| ProcessResponse {
            process_name: data.process_id.clone(),
            pow_contrib_perc: data.pow_perc,
            iteration_metrics: data.iteration_metrics.clone(),
        })
        .collect_vec();

    Ok(Json(RunResponse {
        region: model_data.region.clone(),
        start_time: model_data.start_time,
        duration: model_data.duration(),
        pow: model_data.data.pow,
        co2: model_data.data.co2,
        ci: model_data.ci,
        processes,
    }))
}

pub async fn run_daemon(
    cpu_id: i32,
    region: &Option<String>,
    ci: f64,
    processes_to_observe: Vec<ProcessToObserve>,
    db: DatabaseConnection,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::channel::<Signal>(10);

    let region = region.clone();
    let db_clone = db.clone();

    tokio::spawn(async move {
        loop {
            let mut run_id: String = "".to_string();
            let mut start_resp_tx: mpsc::Sender<Result<(), ServerError>>;

            // wait for signal to start recording
            // TODO: what if multiple "start" signals are sent? This needs more thought!
            while let Some(signal) = rx.recv().await {
                if let Signal::Start {
                    run_id: id,
                    resp_tx,
                } = signal
                {
                    // TODO: do a check to see if the run id already exists
                    //       if it does then reject this signal with an err
                    run_id = id.clone();
                    start_resp_tx = resp_tx;
                    println!("> starting run {}", id.clone());
                    break;
                }
            }

            let start_time = Utc::now().timestamp_millis();

            // upsert run
            let txn = db.begin().await.unwrap();
            let active_run = match entities::run::Entity::find_by_id(&run_id).one(&txn).await {
                Ok(active_run) => active_run,
                Err(err) => {
                    start_resp_tx.send(Err(ServerError(anyhow::anyhow!(""))));
                    break;
                }
            };

            let mut active_run = if active_run.is_none() {
                entities::run::ActiveModel {
                    id: ActiveValue::Set(run_id.clone()),
                    is_live: ActiveValue::Set(true),
                    cpu_id: ActiveValue::Set(cpu_id),
                    region: ActiveValue::Set(region.clone()),
                    carbon_intensity: ActiveValue::Set(ci),
                    start_time: ActiveValue::Set(start_time),
                    stop_time: ActiveValue::set(None),
                }
                .insert(&txn)
                .await
                .unwrap()
                .into_active_model()
            } else {
                // this unwrap is safe
                active_run.unwrap().into_active_model()
            };

            // upsert iteration
            let active_iteration = entities::iteration::Entity
                .select()
                .filter(entities::iteration::Column::RunId.eq(&run_id))
                .one(&txn)
                .await
                .unwrap();
            let mut active_iteration = if active_iteration.is_none() {
                entities::iteration::ActiveModel {
                    id: ActiveValue::NotSet,
                    run_id: ActiveValue::Set(run_id.clone()),
                    scenario_name: ActiveValue::Set("live".to_string()),
                    count: ActiveValue::Set(1),
                    start_time: ActiveValue::Set(start_time),
                    stop_time: ActiveValue::Set(None), // same as start for now, will be updated later
                }
                .save(&txn)
                .await
                .unwrap()
            } else {
                active_iteration.unwrap().into_active_model()
            };

            txn.commit().await.unwrap();

            // start metric logger
            let stop_handle = metrics_logger::start_logging(
                processes_to_observe.clone(),
                run_id.clone(),
                db.clone(),
            )
            .unwrap(); // TODO: remove unwrap!

            // wait until stop signal is received
            while let Some(signal) = rx.recv().await {
                if let Signal::Stop { resp_tx } = signal {
                    println!("Stopping!");
                    break;
                }
            }

            stop_handle.stop().await;

            // update the iteration stop time
            let now = Utc::now().timestamp_millis();
            active_iteration.stop_time = ActiveValue::Set(Some(now));
            active_iteration.update(&db).await.unwrap();

            // update the run stop time
            active_run.stop_time = ActiveValue::Set(Some(now));
            active_run.clone().update(&db).await.unwrap(); // TODO: remove unwrap!
        }
    });

    // start signal server
    let app = Router::new()
        .route("/start", get(start))
        .route("/stop", get(stop))
        .with_state(ServerState {
            tx: tx.clone(),
            db: db_clone,
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
