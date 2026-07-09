use axum::http::StatusCode;
use uuid::Uuid;

use crate::{
    db::get_instance,
    error::{AppError, AppResult},
    models::{AgentOutbound, CommandJobRecord},
    state::AppState,
    utils::now_ts,
};

pub async fn create_command_job(
    state: &AppState,
    command_id: Option<String>,
    instance_id: &str,
    command: &str,
    requested_by: &str,
) -> AppResult<CommandJobRecord> {
    get_instance(&state.db, instance_id).await?;
    let job = CommandJobRecord {
        id: Uuid::new_v4().to_string(),
        command_id,
        instance_id: instance_id.to_string(),
        command: command.to_string(),
        status: "queued".to_string(),
        requested_by: requested_by.to_string(),
        created_at: now_ts(),
        completed_at: None,
        output: String::new(),
        exit_code: None,
    };

    sqlx::query(
        r#"
        INSERT INTO command_jobs(id, command_id, instance_id, command, status, requested_by,
                                 created_at, completed_at, output, exit_code)
        VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&job.id)
    .bind(&job.command_id)
    .bind(&job.instance_id)
    .bind(&job.command)
    .bind(&job.status)
    .bind(&job.requested_by)
    .bind(job.created_at)
    .bind(job.completed_at)
    .bind(&job.output)
    .bind(job.exit_code)
    .execute(&state.db)
    .await?;

    Ok(job)
}

pub async fn dispatch_command(
    state: &AppState,
    job_id: &str,
    instance_id: &str,
    command: &str,
) -> AppResult<()> {
    let Some(handle) = state.agents.read().await.get(instance_id).cloned() else {
        complete_command_job(state, job_id, -1, "实例不在线，无法下发命令").await?;
        return Err(AppError::new(StatusCode::CONFLICT, "实例不在线"));
    };

    sqlx::query("UPDATE command_jobs SET status = 'running' WHERE id = ?")
        .bind(job_id)
        .execute(&state.db)
        .await?;

    handle
        .tx
        .send(AgentOutbound::RunCommand {
            job_id: job_id.to_string(),
            command: command.to_string(),
        })
        .map_err(|_| AppError::new(StatusCode::CONFLICT, "实例连接已断开"))?;

    Ok(())
}

pub async fn complete_command_job(
    state: &AppState,
    job_id: &str,
    exit_code: i64,
    output: &str,
) -> AppResult<()> {
    let status = if exit_code == 0 {
        "completed"
    } else {
        "failed"
    };
    sqlx::query(
        r#"
        UPDATE command_jobs
        SET status = ?, completed_at = ?, output = ?, exit_code = ?
        WHERE id = ?
        "#,
    )
    .bind(status)
    .bind(now_ts())
    .bind(output)
    .bind(exit_code)
    .bind(job_id)
    .execute(&state.db)
    .await?;
    Ok(())
}
