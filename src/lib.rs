pub mod carbon_intensity;
pub mod config;
pub mod dao;
pub mod data;
pub mod entities;
pub mod execution_modes;
pub mod execution_plan;
pub mod metrics;
pub mod metrics_logger;
pub mod migrations;
pub mod models;
pub mod process_control;
pub mod server;

use crate::{
    execution_modes::{live_monitor::run_live, scenario_runner::run_scenarios, ExecutionMode},
    execution_plan::ExecutionPlan,
    migrations::{Migrator, MigratorTrait},
    process_control::{run_process, shutdown_processes},
};
use anyhow::Context;
use colored::Colorize;
use config::{Power, Process};
use entities::cpu;
use execution_modes::daemon::run_daemon;
use sea_orm::*;
use std::{
    collections::HashMap,
    fs::{self},
    io::Write,
    path::Path,
    process::exit,
};
use tracing::debug;

pub async fn db_connect(
    database_url: &str,
    database_name: Option<&str>,
) -> anyhow::Result<DatabaseConnection> {
    let db = Database::connect(database_url).await?;
    match db.get_database_backend() {
        DbBackend::Sqlite => Ok(db),

        DbBackend::Postgres => {
            let database_name =
                database_name.context("Database name is required for postgres connections")?;
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!("CREATE DATABASE \"{}\";", database_name),
            ))
            .await
            .ok();

            let url = format!("{}/{}", database_url, database_name);
            Database::connect(&url)
                .await
                .context("Error creating postgresql database.")
        }

        DbBackend::MySql => {
            let database_name =
                database_name.context("Database name is required for mysql connections")?;
            db.execute(Statement::from_string(
                db.get_database_backend(),
                format!("CREATE DATABASE IF NOT EXISTS `{}`;", database_name),
            ))
            .await?;

            let url = format!("{}/{}", database_url, database_name);
            Database::connect(&url)
                .await
                .context("Error creating mysql database.")
        }
    }
}

pub async fn db_migrate(db_conn: &DatabaseConnection) -> anyhow::Result<()> {
    Migrator::up(db_conn, None)
        .await
        .context("Error migrating database.")
}

/// Deletes previous runs .stdout and .stderr
/// Stdout and stderr capturing are append due to a scenario / observeration removing previous ones
/// stdout and err
pub fn cleanup_stdout_stderr() -> anyhow::Result<()> {
    debug!("Cleaning up stdout and stderr");
    let stdout = Path::new("./.stdout");
    let stderr = Path::new("./.stderr");
    if stdout.exists() {
        fs::remove_file(stdout)?;
    }
    if stderr.exists() {
        fs::remove_file(stderr)?;
    }
    Ok(())
}

pub fn topological_sort(graph: &mut HashMap<String, Vec<String>>) -> anyhow::Result<Vec<String>> {
    // verify all the processes exist
    for (proc_name, deps) in graph.iter() {
        let exists = deps.iter().all(|dep| graph.contains_key(dep));
        if !exists {
            return Err(anyhow::anyhow!(
                "Process {} depends on another process which doesn't exist",
                proc_name
            ));
        }
    }

    let mut sorted: Vec<String> = vec![];
    while graph.len() > 0 {
        // find processes without deps
        let roots = graph
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(proc_name, _)| (*proc_name).clone())
            .collect::<Vec<_>>();

        // if roots is empty then the graph must contain cycles
        if roots.is_empty() {
            return Err(anyhow::anyhow!("Process graph contains a cycle"));
        }

        // add roots to the sorted list
        sorted.extend(roots.clone());

        // remove roots from graph
        for root in roots {
            graph.remove(&root);
            for (_, deps) in graph.iter_mut() {
                if let Some(pos) = deps.iter().position(|dep| dep == &root) {
                    deps.remove(pos);
                }
            }
        }
    }

    Ok(sorted)
}

pub fn topological_sort_processes(procs: Vec<Process>) -> anyhow::Result<Vec<String>> {
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    for proc in procs {
        if graph.contains_key(&proc.name) {
            return Err(anyhow::anyhow!("Duplicate process named {}", &proc.name));
        }

        for dep in proc.deps.iter() {
            graph
                .entry(proc.name.clone())
                .or_insert_with(Vec::new)
                .push(dep.clone());
        }
    }

    topological_sort(&mut graph)
}

pub async fn run(
    exec_plan: ExecutionPlan<'_>,
    region: Option<String>,
    ci: f64,
    db: DatabaseConnection,
) -> anyhow::Result<()> {
    let mut processes_to_observe = exec_plan.external_processes_to_observe.unwrap_or(vec![]); // external procs to observe are cloned here.

    // run the application if there is anything to run
    if !exec_plan.processes_to_execute.is_empty() {
        for proc in exec_plan.processes_to_execute {
            print!("> starting process {}", proc.name.green());

            let process_to_observe = run_process(proc)?;

            // add process_to_observe to the observation list
            processes_to_observe.push(process_to_observe);
            println!("{}", "\t✓".green());
            println!("\t{}", format!("- {}", proc.up).bright_black());
        }
    }

    print!("> waiting for application to settle");
    std::io::stdout().flush()?;
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
    println!(" {}", "\t✓".green());

    // check if the processor already exists in the db.
    // If it does then reuse it for this run else save
    // a new one
    let cpu = cpu::Entity::find()
        .filter(cpu::Column::Name.eq(&exec_plan.cpu.name))
        .one(&db)
        .await?;

    let cpu_id = match cpu {
        Some(cpu) => cpu.id,
        None => {
            let cpu = match exec_plan.cpu.power {
                Power::Tdp(tdp) => {
                    cpu::ActiveModel {
                        id: ActiveValue::NotSet,
                        name: ActiveValue::Set(exec_plan.cpu.name),
                        tdp: ActiveValue::Set(Some(tdp)),
                        power_curve_id: ActiveValue::NotSet,
                    }
                    .save(&db)
                    .await
                }

                Power::Curve(a, b, c, d) => {
                    let power_curve = entities::power_curve::ActiveModel {
                        id: ActiveValue::NotSet,
                        a: ActiveValue::Set(a),
                        b: ActiveValue::Set(b),
                        c: ActiveValue::Set(c),
                        d: ActiveValue::Set(d),
                    }
                    .save(&db)
                    .await?
                    .try_into_model()?;

                    cpu::ActiveModel {
                        id: ActiveValue::NotSet,
                        name: ActiveValue::Set(exec_plan.cpu.name),
                        tdp: ActiveValue::NotSet,
                        power_curve_id: ActiveValue::Set(Some(power_curve.id)),
                    }
                    .save(&db)
                    .await
                }
            }?;

            cpu.try_into_model()?.id
        }
    };

    match exec_plan.execution_mode {
        ExecutionMode::Observation(scenarios) => {
            run_scenarios(
                cpu_id,
                region,
                ci,
                scenarios,
                processes_to_observe.clone(),
                db.clone(),
            )
            .await?;
        }

        ExecutionMode::Live => {
            run_live(cpu_id, region, ci, processes_to_observe.clone(), db.clone()).await?;
        }

        ExecutionMode::Daemon => {
            run_daemon(cpu_id, region, ci, processes_to_observe.clone(), db.clone()).await?;
        }
    };

    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub async fn setup_fixtures(fixtures: &[&str], db: &DatabaseConnection) -> anyhow::Result<()> {
        for path in fixtures {
            let path = Path::new(path);
            let stmt = std::fs::read_to_string(path)?;
            db.query_one(Statement::from_string(DatabaseBackend::Sqlite, stmt))
                .await
                .context(format!("Error applying fixture {:?}", path))?;
        }

        Ok(())
    }

    #[test]
    pub fn topological_sort_works() -> anyhow::Result<()> {
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        graph.insert("A".to_string(), vec!["B".to_string(), "C".to_string()]);
        graph.insert("B".to_string(), vec!["C".to_string()]);
        graph.insert("C".to_string(), vec![]);

        let sorted = topological_sort(&mut graph)?;
        assert_eq!(
            sorted,
            vec!["C".to_string(), "B".to_string(), "A".to_string()]
        );

        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        graph.insert("A".to_string(), vec!["B".to_string()]);
        graph.insert("B".to_string(), vec!["C".to_string()]);
        graph.insert("C".to_string(), vec!["B".to_string()]);

        let sorted = topological_sort(&mut graph);
        assert!(sorted.is_err());

        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        graph.insert("A".to_string(), vec!["B".to_string(), "C".to_string()]);
        graph.insert("B".to_string(), vec![]);
        graph.insert("C".to_string(), vec!["D".to_string(), "E".to_string()]);
        graph.insert("D".to_string(), vec!["F".to_string()]);
        graph.insert("E".to_string(), vec![]);
        graph.insert("F".to_string(), vec![]);

        let mut sorted = topological_sort(&mut graph)?;
        let (first, last) = sorted.split_at_mut(3);
        first.sort();
        assert_eq!(first, ["B".to_string(), "E".to_string(), "F".to_string()]);
        assert_eq!(last, ["D".to_string(), "C".to_string(), "A".to_string()]);

        Ok(())
    }
}
