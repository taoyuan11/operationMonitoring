use std::time::Duration;

use axum::http::StatusCode;
use tracing::error;
use uuid::Uuid;

use crate::{
    db::get_instance,
    error::{AppError, AppResult},
    models::{AgentOutbound, CommandJobRecord},
    state::AppState,
    utils::now_ts,
};

const COMMAND_TIMEOUT_SECONDS: i64 = 60 * 60;
const COMMAND_TIMEOUT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

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
        VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
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
    let connection_id = state
        .agents
        .read()
        .await
        .get(instance_id)
        .map(|handle| handle.connection_id);
    let Some(connection_id) = connection_id else {
        fail_queued_command_job(state, job_id, instance_id, "实例不在线，无法下发命令").await?;
        return Err(AppError::new(StatusCode::CONFLICT, "实例不在线"));
    };

    let started_at = now_ts();
    let started = sqlx::query(
        r#"
        UPDATE command_jobs
        SET status = 'running', started_at = $1, agent_connection_id = $2
        WHERE id = $3 AND instance_id = $4 AND status = 'queued'
        "#,
    )
    .bind(started_at)
    .bind(connection_id.to_string())
    .bind(job_id)
    .bind(instance_id)
    .execute(&state.db)
    .await?;
    if started.rows_affected() != 1 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "命令任务状态已发生变化",
        ));
    }

    let sent = {
        let agents = state.agents.read().await;
        agents
            .get(instance_id)
            .filter(|handle| handle.connection_id == connection_id)
            .is_some_and(|handle| {
                handle
                    .tx
                    .send(AgentOutbound::RunCommand {
                        job_id: job_id.to_string(),
                        command: command.to_string(),
                    })
                    .is_ok()
            })
    };
    if !sent {
        fail_running_command_job(
            state,
            job_id,
            instance_id,
            connection_id,
            "实例连接已断开，命令未下发",
        )
        .await?;
        return Err(AppError::new(StatusCode::CONFLICT, "实例连接已断开"));
    }

    Ok(())
}

pub async fn complete_command_job(
    state: &AppState,
    job_id: &str,
    instance_id: &str,
    connection_id: Uuid,
    exit_code: i64,
    output: &str,
) -> AppResult<bool> {
    let result = sqlx::query(
        r#"
        UPDATE command_jobs
        SET status = $1, completed_at = $2, output = $3, exit_code = $4
        WHERE id = $5 AND instance_id = $6 AND agent_connection_id = $7
          AND status = 'running'
        "#,
    )
    .bind(completion_status(exit_code))
    .bind(now_ts())
    .bind(output)
    .bind(exit_code)
    .bind(job_id)
    .bind(instance_id)
    .bind(connection_id.to_string())
    .execute(&state.db)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn fail_connection_command_jobs(
    state: &AppState,
    instance_id: &str,
    connection_id: Uuid,
) -> AppResult<u64> {
    let result = sqlx::query(
        r#"
        UPDATE command_jobs
        SET status = 'failed', completed_at = $1, output = $2, exit_code = -1
        WHERE instance_id = $3 AND agent_connection_id = $4 AND status = 'running'
        "#,
    )
    .bind(now_ts())
    .bind("实例连接已断开，命令结果未知")
    .bind(instance_id)
    .bind(connection_id.to_string())
    .execute(&state.db)
    .await?;
    Ok(result.rows_affected())
}

pub async fn command_timeout_loop(state: AppState) {
    let mut interval = tokio::time::interval(COMMAND_TIMEOUT_CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(error) = expire_command_jobs(&state).await {
            error!(?error, "failed to expire timed-out command jobs");
        }
    }
}

async fn expire_command_jobs(state: &AppState) -> AppResult<u64> {
    let now = now_ts();
    let result = sqlx::query(
        r#"
        UPDATE command_jobs
        SET status = 'failed', completed_at = $1, output = $2, exit_code = -1
        WHERE status = 'running' AND started_at < $3
        "#,
    )
    .bind(now)
    .bind("命令执行超过 60 分钟，结果未知")
    .bind(command_timeout_cutoff(now))
    .execute(&state.db)
    .await?;
    Ok(result.rows_affected())
}

async fn fail_queued_command_job(
    state: &AppState,
    job_id: &str,
    instance_id: &str,
    message: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE command_jobs
        SET status = 'failed', completed_at = $1, output = $2, exit_code = -1
        WHERE id = $3 AND instance_id = $4 AND status = 'queued'
        "#,
    )
    .bind(now_ts())
    .bind(message)
    .bind(job_id)
    .bind(instance_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn fail_running_command_job(
    state: &AppState,
    job_id: &str,
    instance_id: &str,
    connection_id: Uuid,
    message: &str,
) -> AppResult<()> {
    sqlx::query(
        r#"
        UPDATE command_jobs
        SET status = 'failed', completed_at = $1, output = $2, exit_code = -1
        WHERE id = $3 AND instance_id = $4 AND agent_connection_id = $5
          AND status = 'running'
        "#,
    )
    .bind(now_ts())
    .bind(message)
    .bind(job_id)
    .bind(instance_id)
    .bind(connection_id.to_string())
    .execute(&state.db)
    .await?;
    Ok(())
}

fn completion_status(exit_code: i64) -> &'static str {
    if exit_code == 0 {
        "completed"
    } else {
        "failed"
    }
}

fn command_timeout_cutoff(now: i64) -> i64 {
    now.saturating_sub(COMMAND_TIMEOUT_SECONDS)
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf};

    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::{auth::AuthCipher, config::Cli, db::init_db};

    #[test]
    fn command_completion_status_tracks_exit_code() {
        assert_eq!(completion_status(0), "completed");
        assert_eq!(completion_status(1), "failed");
        assert_eq!(completion_status(-1), "failed");
    }

    #[test]
    fn command_timeout_cutoff_saturates() {
        assert_eq!(command_timeout_cutoff(COMMAND_TIMEOUT_SECONDS + 10), 10);
        assert_eq!(command_timeout_cutoff(i64::MIN), i64::MIN);
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn command_results_require_matching_running_job_connection() {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect("postgresql://localhost/postgres")
            .await
            .expect("connect database");
        init_db(&db).await.expect("initialize database");
        let state = AppState::new(
            db,
            Cli {
                bind: "127.0.0.1:0".parse::<SocketAddr>().expect("bind address"),
                database_url: "postgresql://localhost/postgres".to_string(),
                database_password: None,
                admin_password: Some("test-bootstrap-password".to_string()),
                auth_secret_key: None,
                auth_key_file: PathBuf::from("unused-test-auth-key"),
                secure_cookies: false,
                trust_proxy_headers: false,
                trusted_proxy_cidrs: Vec::new(),
                allow_legacy_agent_ws_auth: false,
                reset_admin_auth: false,
                confirm_reset_admin_auth: None,
                upload_dir: PathBuf::from("unused-uploads"),
                update_dir: PathBuf::from("unused-updates"),
                agent_package_max_bytes: 1024,
                file_transfer_max_bytes: 1024,
            },
            AuthCipher::from_key(&[3_u8; 32]).expect("create auth cipher"),
        );
        let job_id = Uuid::new_v4().to_string();
        let terminal_job_id = Uuid::new_v4().to_string();
        let connection_id = Uuid::new_v4();
        for (id, status) in [(&job_id, "running"), (&terminal_job_id, "completed")] {
            sqlx::query(
                r#"
                INSERT INTO command_jobs(
                    id, instance_id, command, status, requested_by, created_at, started_at,
                    completed_at, output, exit_code, agent_connection_id
                ) VALUES($1, 'agent-a', 'true', $2, 'test', $3, $3, NULL, '', NULL, $4)
                "#,
            )
            .bind(id)
            .bind(status)
            .bind(now_ts())
            .bind(connection_id.to_string())
            .execute(&state.db)
            .await
            .expect("insert command job");
        }

        assert!(
            !complete_command_job(
                &state,
                &job_id,
                "agent-b",
                connection_id,
                0,
                "wrong instance",
            )
            .await
            .expect("reject mismatched instance")
        );
        assert!(
            !complete_command_job(
                &state,
                &job_id,
                "agent-a",
                Uuid::new_v4(),
                0,
                "wrong connection",
            )
            .await
            .expect("reject mismatched connection")
        );
        assert!(
            complete_command_job(&state, &job_id, "agent-a", connection_id, 0, "done")
                .await
                .expect("accept matching result")
        );
        assert!(
            !complete_command_job(
                &state,
                &terminal_job_id,
                "agent-a",
                connection_id,
                0,
                "late result",
            )
            .await
            .expect("reject terminal job result")
        );

        sqlx::query("DELETE FROM command_jobs WHERE id = $1 OR id = $2")
            .bind(&job_id)
            .bind(&terminal_job_id)
            .execute(&state.db)
            .await
            .expect("delete command jobs");
    }
}
