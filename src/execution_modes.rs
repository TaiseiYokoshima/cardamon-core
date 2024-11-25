pub mod daemon;
pub mod live_monitor;
pub mod scenario_runner;

use crate::{config::Scenario, entities};
use anyhow::Context;
use sea_orm::*;

#[derive(Debug)]
pub enum ExecutionMode<'a> {
    Live,
    Observation(Vec<&'a Scenario>),
    Daemon,
}

/// Sets the stop time for the given run and all it's iterations
async fn finalise_run(run_id: i32, stop_time: i64, db: &DatabaseConnection) -> anyhow::Result<()> {
    // update run
    let mut run = entities::run::Entity::find_by_id(run_id)
        .one(db)
        .await?
        .context(format!("Expected to find a run with id {}", run_id))?
        .into_active_model();

    run.stop_time = ActiveValue::Set(Some(stop_time));
    run.save(db).await?;

    // update iterations
    let mut iterations = entities::iteration::Entity
        .select()
        .filter(Condition::all().add(entities::iteration::Column::RunId.eq(run_id)))
        .all(db)
        .await?;
    for iteration in iterations.iter_mut() {
        let mut active_model = iteration.clone().into_active_model();
        active_model.stop_time = ActiveValue::Set(Some(stop_time));
        active_model.save(db).await?;
    }

    Ok(())
}
