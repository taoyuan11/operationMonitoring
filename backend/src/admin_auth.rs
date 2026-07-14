use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::{Executor, FromRow, PgPool, Postgres};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{
    auth::{
        AdminPrincipal, ENROLLMENT_MAX_AGE, SESSION_COOKIE, SESSION_MAX_AGE, generate_totp_secret,
        insert_session, otpauth_uri, require_admin, session_token, validate_username, verify_totp,
    },
    db::write_action_log,
    error::{AppError, AppResult},
    state::AppState,
    utils::now_ts,
};

const AUTH_FAILURE_LIMIT: u32 = 5;
const AUTH_WINDOW_SECONDS: i64 = 5 * 60;
const AUTH_BLOCK_SECONDS: i64 = 5 * 60;

#[derive(Serialize)]
pub struct AuthStatusResponse {
    mode: &'static str,
}

#[derive(Deserialize)]
pub struct BootstrapStartRequest {
    password: String,
    username: String,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    code: String,
}

#[derive(Deserialize)]
pub struct ConfirmEnrollmentRequest {
    code: String,
}

#[derive(Deserialize)]
pub struct CreateUserEnrollmentRequest {
    username: String,
    current_code: String,
}

#[derive(Deserialize)]
pub struct StepUpRequest {
    current_code: String,
}

#[derive(Deserialize)]
pub struct SetUserEnabledRequest {
    enabled: bool,
    current_code: String,
}

#[derive(Serialize)]
pub struct EnrollmentResponse {
    id: String,
    username: String,
    device_name: String,
    otpauth_uri: String,
    expires_at: i64,
}

#[derive(Serialize)]
pub struct SessionUserResponse {
    id: String,
    username: String,
}

#[derive(Serialize)]
pub struct MeResponse {
    authenticated: bool,
    user: Option<SessionUserResponse>,
}

#[derive(Serialize)]
pub struct LoginResponse {
    role: &'static str,
    user: SessionUserResponse,
}

#[derive(Serialize)]
pub struct AdminDeviceResponse {
    id: String,
    name: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

#[derive(Serialize)]
pub struct AdminUserResponse {
    id: String,
    username: String,
    enabled: bool,
    created_at: i64,
    devices: Vec<AdminDeviceResponse>,
}

#[derive(Serialize)]
pub struct PendingEnrollmentResponse {
    id: String,
    target_user_id: Option<String>,
    username: String,
    device_name: String,
    created_at: i64,
    expires_at: i64,
}

#[derive(Serialize)]
pub struct UsersResponse {
    users: Vec<AdminUserResponse>,
    enrollments: Vec<PendingEnrollmentResponse>,
}

#[derive(FromRow)]
struct UserRow {
    id: String,
    username: String,
    enabled: i64,
    created_at: i64,
}

#[derive(FromRow)]
struct DeviceRow {
    id: String,
    user_id: String,
    name: String,
    secret_ciphertext: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

#[derive(FromRow)]
struct EnrollmentRow {
    id: String,
    target_user_id: Option<String>,
    username: String,
    username_normalized: String,
    device_name: String,
    secret_ciphertext: String,
    created_by_user_id: Option<String>,
    created_at: i64,
    expires_at: i64,
}

pub async fn auth_status(State(state): State<AppState>) -> AppResult<Json<AuthStatusResponse>> {
    let initialized = auth_initialized(&state.db).await?;
    Ok(Json(AuthStatusResponse {
        mode: if initialized { "totp" } else { "bootstrap" },
    }))
}

pub async fn bootstrap_start(
    State(state): State<AppState>,
    Json(payload): Json<BootstrapStartRequest>,
) -> AppResult<Response> {
    ensure_attempt_allowed(&state, "bootstrap").await?;
    if auth_initialized(&state.db).await? {
        return Err(AppError::new(StatusCode::CONFLICT, "管理员认证已经初始化"));
    }
    if !bool::from(
        state
            .admin_password
            .as_bytes()
            .ct_eq(payload.password.as_bytes()),
    ) {
        record_auth_failure(&state, "bootstrap").await;
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "初始化凭据错误"));
    }
    let (username, normalized) = validate_username(&payload.username)?;
    clear_auth_failures(&state, "bootstrap").await;
    remove_expired_enrollments(&state.db).await?;

    let secret = generate_totp_secret();
    let ciphertext = state.auth_cipher.encrypt(&secret)?;
    let now = now_ts();
    let enrollment_id = Uuid::new_v4().to_string();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM admin_enrollments WHERE target_user_id IS NULL")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO admin_enrollments(
            id, target_user_id, username, username_normalized, device_name,
            secret_ciphertext, created_by_user_id, created_at, expires_at
        ) VALUES($1, NULL, $2, $3, '认证器 1', $4, NULL, $5, $6)
        "#,
    )
    .bind(&enrollment_id)
    .bind(&username)
    .bind(&normalized)
    .bind(ciphertext)
    .bind(now)
    .bind(now + ENROLLMENT_MAX_AGE)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    no_store_json(EnrollmentResponse {
        id: enrollment_id,
        username: username.clone(),
        device_name: "认证器 1".to_string(),
        otpauth_uri: otpauth_uri(&username, &secret),
        expires_at: now + ENROLLMENT_MAX_AGE,
    })
}

pub async fn bootstrap_confirm(
    State(state): State<AppState>,
    Path(enrollment_id): Path<String>,
    Json(payload): Json<ConfirmEnrollmentRequest>,
) -> AppResult<Response> {
    ensure_attempt_allowed(&state, "bootstrap-confirm").await?;
    if auth_initialized(&state.db).await? {
        return Err(AppError::new(StatusCode::CONFLICT, "管理员认证已经初始化"));
    }
    let enrollment = load_enrollment(&state.db, &enrollment_id).await?;
    if enrollment.target_user_id.is_some()
        || enrollment.created_by_user_id.is_some()
        || enrollment.expires_at <= now_ts()
    {
        return Err(AppError::bad_request("二维码已过期，请重新开始初始化"));
    }
    if !verify_enrollment_code(&state, &enrollment, &payload.code)? {
        record_auth_failure(&state, "bootstrap-confirm").await;
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "验证码错误"));
    }

    let user_id = Uuid::new_v4().to_string();
    let device_id = Uuid::new_v4().to_string();
    let now = now_ts();
    let mut tx = state.db.begin().await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_users")
        .fetch_one(&mut *tx)
        .await?;
    if count != 0 {
        return Err(AppError::new(StatusCode::CONFLICT, "管理员认证已经初始化"));
    }
    sqlx::query(
        "INSERT INTO admin_users(id, username, username_normalized, enabled, created_at) VALUES($1, $2, $3, 1, $4)",
    )
    .bind(&user_id)
    .bind(&enrollment.username)
    .bind(&enrollment.username_normalized)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO authenticator_devices(id, user_id, name, secret_ciphertext, created_at, last_used_at) VALUES($1, $2, $3, $4, $5, $6)",
    )
    .bind(&device_id)
    .bind(&user_id)
    .bind(&enrollment.device_name)
    .bind(&enrollment.secret_ciphertext)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM admin_enrollments")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    clear_auth_failures(&state, "bootstrap-confirm").await;
    write_action_log(
        &state.db,
        &enrollment.username,
        "initialize_auth",
        &user_id,
        "初始化管理员 Authenticator 认证",
    )
    .await?;
    session_response(&state, user_id, enrollment.username, device_id).await
}

pub async fn admin_login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> AppResult<Response> {
    if !auth_initialized(&state.db).await? {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "请先使用默认密码完成管理员初始化",
        ));
    }
    let normalized = payload.username.trim().to_ascii_lowercase();
    let attempt_key = format!("login:{normalized}");
    ensure_attempt_allowed(&state, &attempt_key).await?;
    let user = sqlx::query_as::<_, UserRow>(
        "SELECT id, username, enabled, created_at FROM admin_users WHERE username_normalized = $1",
    )
    .bind(&normalized)
    .fetch_optional(&state.db)
    .await?;
    let authenticated = if let Some(user) = user.filter(|user| user.enabled == 1) {
        verify_user_code(&state, &user.id, &payload.code)
            .await?
            .map(|device| (user, device))
    } else {
        None
    };
    let Some((user, device)) = authenticated else {
        record_auth_failure(&state, &attempt_key).await;
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "用户名或验证码错误",
        ));
    };
    clear_auth_failures(&state, &attempt_key).await;
    sqlx::query("UPDATE authenticator_devices SET last_used_at = $1 WHERE id = $2")
        .bind(now_ts())
        .bind(&device.id)
        .execute(&state.db)
        .await?;
    write_action_log(&state.db, &user.username, "login", &user.id, "管理员登录").await?;
    session_response(&state, user.id, user.username, device.id).await
}

pub async fn admin_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Response> {
    if let Some(token) = session_token(&headers) {
        state.sessions.write().await.remove(&token);
    }
    let mut response = Json(MeResponse {
        authenticated: false,
        user: None,
    })
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("om_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
    );
    Ok(response)
}

pub async fn admin_me(State(state): State<AppState>, headers: HeaderMap) -> Json<MeResponse> {
    match require_admin(&state, &headers).await {
        Ok(principal) => Json(MeResponse {
            authenticated: true,
            user: Some(SessionUserResponse {
                id: principal.user_id,
                username: principal.username,
            }),
        }),
        Err(_) => Json(MeResponse {
            authenticated: false,
            user: None,
        }),
    }
}

pub async fn admin_users(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<UsersResponse>> {
    require_admin(&state, &headers).await?;
    remove_expired_enrollments(&state.db).await?;
    let rows = sqlx::query_as::<_, UserRow>(
        "SELECT id, username, enabled, created_at FROM admin_users ORDER BY LOWER(username)",
    )
    .fetch_all(&state.db)
    .await?;
    let mut users = Vec::with_capacity(rows.len());
    for row in rows {
        let devices = sqlx::query_as::<_, DeviceRow>(
            "SELECT id, user_id, name, secret_ciphertext, created_at, last_used_at FROM authenticator_devices WHERE user_id = $1 ORDER BY created_at",
        )
        .bind(&row.id)
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|device| AdminDeviceResponse {
            id: device.id,
            name: device.name,
            created_at: device.created_at,
            last_used_at: device.last_used_at,
        })
        .collect();
        users.push(AdminUserResponse {
            id: row.id,
            username: row.username,
            enabled: row.enabled == 1,
            created_at: row.created_at,
            devices,
        });
    }
    let enrollments = sqlx::query_as::<_, EnrollmentRow>(
        r#"
        SELECT id, target_user_id, username, username_normalized, device_name,
               secret_ciphertext, created_by_user_id, created_at, expires_at
        FROM admin_enrollments WHERE expires_at > $1 ORDER BY created_at DESC
        "#,
    )
    .bind(now_ts())
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|row| PendingEnrollmentResponse {
        id: row.id,
        target_user_id: row.target_user_id,
        username: row.username,
        device_name: row.device_name,
        created_at: row.created_at,
        expires_at: row.expires_at,
    })
    .collect();
    Ok(Json(UsersResponse { users, enrollments }))
}

pub async fn create_user_enrollment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateUserEnrollmentRequest>,
) -> AppResult<Response> {
    let principal = require_admin(&state, &headers).await?;
    verify_step_up(&state, &principal, &payload.current_code).await?;
    let (username, normalized) = validate_username(&payload.username)?;
    remove_expired_enrollments(&state.db).await?;
    let exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM admin_users WHERE username_normalized = $1")
            .bind(&normalized)
            .fetch_one(&state.db)
            .await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM admin_enrollments WHERE username_normalized = $1 AND expires_at > $2",
    )
    .bind(&normalized)
    .bind(now_ts())
    .fetch_one(&state.db)
    .await?;
    if exists != 0 || pending != 0 {
        return Err(AppError::new(
            StatusCode::CONFLICT,
            "用户名已存在或正在等待确认",
        ));
    }
    let response = create_enrollment(&state, None, &username, &normalized, &principal).await?;
    write_action_log(
        &state.db,
        &principal.username,
        "create_user_enrollment",
        &response.id,
        &format!("为 {username} 创建 Authenticator 注册"),
    )
    .await?;
    no_store_json(response)
}

pub async fn create_device_enrollment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(payload): Json<StepUpRequest>,
) -> AppResult<Response> {
    let principal = require_admin(&state, &headers).await?;
    verify_step_up(&state, &principal, &payload.current_code).await?;
    let user = load_user(&state.db, &user_id).await?;
    remove_expired_enrollments(&state.db).await?;
    let response = create_enrollment(
        &state,
        Some(&user.id),
        &user.username,
        &user.username.to_ascii_lowercase(),
        &principal,
    )
    .await?;
    write_action_log(
        &state.db,
        &principal.username,
        "create_device_enrollment",
        &response.id,
        &format!("为 {} 添加 Authenticator 设备", user.username),
    )
    .await?;
    no_store_json(response)
}

pub async fn confirm_admin_enrollment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(enrollment_id): Path<String>,
    Json(payload): Json<ConfirmEnrollmentRequest>,
) -> AppResult<Json<UsersResponse>> {
    let principal = require_admin(&state, &headers).await?;
    let enrollment = load_enrollment(&state.db, &enrollment_id).await?;
    if enrollment.created_by_user_id.is_none() || enrollment.expires_at <= now_ts() {
        return Err(AppError::bad_request("二维码已过期，请重新生成"));
    }
    if !verify_enrollment_code(&state, &enrollment, &payload.code)? {
        return Err(AppError::new(StatusCode::UNAUTHORIZED, "新设备验证码错误"));
    }
    let now = now_ts();
    let device_id = Uuid::new_v4().to_string();
    let mut tx = state.db.begin().await?;
    let user_id = if let Some(user_id) = &enrollment.target_user_id {
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
        if exists == 0 {
            return Err(AppError::new(StatusCode::NOT_FOUND, "目标用户不存在"));
        }
        user_id.clone()
    } else {
        let user_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO admin_users(id, username, username_normalized, enabled, created_at) VALUES($1, $2, $3, 1, $4)",
        )
        .bind(&user_id)
        .bind(&enrollment.username)
        .bind(&enrollment.username_normalized)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            if matches!(&error, sqlx::Error::Database(database) if database.is_unique_violation()) {
                AppError::new(StatusCode::CONFLICT, "用户名已存在")
            } else {
                error.into()
            }
        })?;
        user_id
    };
    sqlx::query(
        "INSERT INTO authenticator_devices(id, user_id, name, secret_ciphertext, created_at, last_used_at) VALUES($1, $2, $3, $4, $5, $6)",
    )
    .bind(&device_id)
    .bind(&user_id)
    .bind(&enrollment.device_name)
    .bind(&enrollment.secret_ciphertext)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM admin_enrollments WHERE id = $1")
        .bind(&enrollment.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    write_action_log(
        &state.db,
        &principal.username,
        "confirm_authenticator",
        &device_id,
        &format!(
            "已确认 {} 的 {}",
            enrollment.username, enrollment.device_name
        ),
    )
    .await?;
    admin_users(State(state), headers).await
}

pub async fn cancel_admin_enrollment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(enrollment_id): Path<String>,
    Json(payload): Json<StepUpRequest>,
) -> AppResult<StatusCode> {
    let principal = require_admin(&state, &headers).await?;
    verify_step_up(&state, &principal, &payload.current_code).await?;
    let enrollment = load_enrollment(&state.db, &enrollment_id).await?;
    sqlx::query("DELETE FROM admin_enrollments WHERE id = $1")
        .bind(&enrollment_id)
        .execute(&state.db)
        .await?;
    write_action_log(
        &state.db,
        &principal.username,
        "cancel_authenticator_enrollment",
        &enrollment_id,
        &format!("取消 {} 的 Authenticator 注册", enrollment.username),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn set_admin_user_enabled(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(payload): Json<SetUserEnabledRequest>,
) -> AppResult<StatusCode> {
    let principal = require_admin(&state, &headers).await?;
    verify_step_up(&state, &principal, &payload.current_code).await?;
    let target = load_user(&state.db, &user_id).await?;
    if !payload.enabled && principal.user_id == target.id {
        return Err(AppError::bad_request("不能停用当前登录用户"));
    }
    let mut transaction = state.db.begin().await?;
    if payload.enabled {
        let devices: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM authenticator_devices WHERE user_id = $1")
                .bind(&target.id)
                .fetch_one(&mut *transaction)
                .await?;
        if devices == 0 {
            return Err(AppError::bad_request("该用户没有可用的 Authenticator 设备"));
        }
    } else {
        ensure_other_login_path(&mut *transaction, Some(&target.id), None).await?;
    }
    sqlx::query("UPDATE admin_users SET enabled = $1 WHERE id = $2")
        .bind(i64::from(payload.enabled))
        .bind(&target.id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    if !payload.enabled {
        purge_user_sessions(&state, &target.id).await;
    }
    write_action_log(
        &state.db,
        &principal.username,
        if payload.enabled {
            "enable_user"
        } else {
            "disable_user"
        },
        &target.id,
        if payload.enabled {
            "启用管理员用户"
        } else {
            "停用管理员用户"
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_admin_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(payload): Json<StepUpRequest>,
) -> AppResult<StatusCode> {
    let principal = require_admin(&state, &headers).await?;
    verify_step_up(&state, &principal, &payload.current_code).await?;
    let target = load_user(&state.db, &user_id).await?;
    if principal.user_id == target.id {
        return Err(AppError::bad_request("不能删除当前登录用户"));
    }
    let mut transaction = state.db.begin().await?;
    if target.enabled == 1 {
        ensure_other_login_path(&mut *transaction, Some(&target.id), None).await?;
    }
    sqlx::query("DELETE FROM admin_users WHERE id = $1")
        .bind(&target.id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    purge_user_sessions(&state, &target.id).await;
    write_action_log(
        &state.db,
        &principal.username,
        "delete_user",
        &target.id,
        &format!("删除管理员用户 {}", target.username),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn revoke_authenticator_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
    Json(payload): Json<StepUpRequest>,
) -> AppResult<StatusCode> {
    let principal = require_admin(&state, &headers).await?;
    verify_step_up(&state, &principal, &payload.current_code).await?;
    let device = sqlx::query_as::<_, DeviceRow>(
        "SELECT id, user_id, name, secret_ciphertext, created_at, last_used_at FROM authenticator_devices WHERE id = $1",
    )
    .bind(&device_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Authenticator 设备不存在"))?;
    let user = load_user(&state.db, &device.user_id).await?;
    let mut transaction = state.db.begin().await?;
    if user.enabled == 1 {
        ensure_other_login_path(&mut *transaction, None, Some(&device.id)).await?;
    }
    sqlx::query("DELETE FROM authenticator_devices WHERE id = $1")
        .bind(&device.id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    state
        .sessions
        .write()
        .await
        .retain(|_, session| session.device_id != device.id);
    write_action_log(
        &state.db,
        &principal.username,
        "revoke_authenticator",
        &device.id,
        &format!("撤销 {} 的 {}", user.username, device.name),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn reset_admin_auth(db: &PgPool) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM admin_enrollments")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM authenticator_devices")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM admin_users")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO action_logs(id, actor, action, target, detail, created_at) VALUES($1, 'system', 'reset_admin_auth', 'authentication', '通过服务器本地命令重置管理员认证', $2)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(now_ts())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn create_enrollment(
    state: &AppState,
    target_user_id: Option<&str>,
    username: &str,
    normalized: &str,
    principal: &AdminPrincipal,
) -> AppResult<EnrollmentResponse> {
    let count: i64 = if let Some(user_id) = target_user_id {
        sqlx::query_scalar("SELECT COUNT(*) FROM authenticator_devices WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&state.db)
            .await?
    } else {
        0
    };
    let device_name = format!("认证器 {}", count + 1);
    let secret = generate_totp_secret();
    let ciphertext = state.auth_cipher.encrypt(&secret)?;
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    sqlx::query(
        r#"
        INSERT INTO admin_enrollments(
            id, target_user_id, username, username_normalized, device_name,
            secret_ciphertext, created_by_user_id, created_at, expires_at
        ) VALUES($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(&id)
    .bind(target_user_id)
    .bind(username)
    .bind(normalized)
    .bind(&device_name)
    .bind(ciphertext)
    .bind(&principal.user_id)
    .bind(now)
    .bind(now + ENROLLMENT_MAX_AGE)
    .execute(&state.db)
    .await?;
    Ok(EnrollmentResponse {
        id,
        username: username.to_string(),
        device_name,
        otpauth_uri: otpauth_uri(username, &secret),
        expires_at: now + ENROLLMENT_MAX_AGE,
    })
}

async fn verify_step_up(state: &AppState, principal: &AdminPrincipal, code: &str) -> AppResult<()> {
    let key = format!("step-up:{}", principal.user_id);
    ensure_attempt_allowed(state, &key).await?;
    if verify_user_code(state, &principal.user_id, code)
        .await?
        .is_none()
    {
        record_auth_failure(state, &key).await;
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "当前管理员验证码错误",
        ));
    }
    clear_auth_failures(state, &key).await;
    Ok(())
}

async fn verify_user_code(
    state: &AppState,
    user_id: &str,
    code: &str,
) -> AppResult<Option<DeviceRow>> {
    let devices = sqlx::query_as::<_, DeviceRow>(
        "SELECT id, user_id, name, secret_ciphertext, created_at, last_used_at FROM authenticator_devices WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    for device in devices {
        let secret = state.auth_cipher.decrypt(&device.secret_ciphertext)?;
        if verify_totp(&secret, code, now_ts()) {
            sqlx::query("UPDATE authenticator_devices SET last_used_at = $1 WHERE id = $2")
                .bind(now_ts())
                .bind(&device.id)
                .execute(&state.db)
                .await?;
            return Ok(Some(device));
        }
    }
    Ok(None)
}

fn verify_enrollment_code(
    state: &AppState,
    enrollment: &EnrollmentRow,
    code: &str,
) -> AppResult<bool> {
    let secret = state.auth_cipher.decrypt(&enrollment.secret_ciphertext)?;
    Ok(verify_totp(&secret, code, now_ts()))
}

async fn auth_initialized(db: &PgPool) -> AppResult<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_users")
        .fetch_one(db)
        .await?;
    Ok(count > 0)
}

async fn load_user(db: &PgPool, user_id: &str) -> AppResult<UserRow> {
    sqlx::query_as::<_, UserRow>(
        "SELECT id, username, enabled, created_at FROM admin_users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "管理员用户不存在"))
}

async fn load_enrollment(db: &PgPool, id: &str) -> AppResult<EnrollmentRow> {
    sqlx::query_as::<_, EnrollmentRow>(
        r#"
        SELECT id, target_user_id, username, username_normalized, device_name,
               secret_ciphertext, created_by_user_id, created_at, expires_at
        FROM admin_enrollments WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| AppError::new(StatusCode::NOT_FOUND, "Authenticator 注册不存在"))
}

async fn remove_expired_enrollments(db: &PgPool) -> AppResult<()> {
    sqlx::query("DELETE FROM admin_enrollments WHERE expires_at <= $1")
        .bind(now_ts())
        .execute(db)
        .await?;
    Ok(())
}

async fn ensure_other_login_path<'e, E>(
    executor: E,
    excluded_user_id: Option<&str>,
    excluded_device_id: Option<&str>,
) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM authenticator_devices d
        JOIN admin_users u ON u.id = d.user_id
        WHERE u.enabled = 1
          AND ($1 IS NULL OR u.id != $2)
          AND ($3 IS NULL OR d.id != $4)
        "#,
    )
    .bind(excluded_user_id)
    .bind(excluded_user_id)
    .bind(excluded_device_id)
    .bind(excluded_device_id)
    .fetch_one(executor)
    .await?;
    if count == 0 {
        return Err(AppError::bad_request("该操作会移除最后一个可登录管理员"));
    }
    Ok(())
}

async fn purge_user_sessions(state: &AppState, user_id: &str) {
    state
        .sessions
        .write()
        .await
        .retain(|_, session| session.user_id != user_id);
}

async fn session_response(
    state: &AppState,
    user_id: String,
    username: String,
    device_id: String,
) -> AppResult<Response> {
    let token = Uuid::new_v4().to_string();
    insert_session(
        state,
        token.clone(),
        user_id.clone(),
        username.clone(),
        device_id,
    )
    .await;
    let mut response = Json(LoginResponse {
        role: "admin",
        user: SessionUserResponse {
            id: user_id,
            username,
        },
    })
    .into_response();
    let secure = if state.secure_cookies { "; Secure" } else { "" };
    let cookie = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_MAX_AGE}{secure}"
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|_| AppError::new(StatusCode::INTERNAL_SERVER_ERROR, "Cookie 生成失败"))?,
    );
    Ok(response)
}

fn no_store_json<T: Serialize>(payload: T) -> AppResult<Response> {
    let mut response = Json(payload).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn ensure_attempt_allowed(state: &AppState, key: &str) -> AppResult<()> {
    let now = now_ts();
    let attempts = state.auth_attempts.read().await;
    if attempts
        .get(key)
        .is_some_and(|attempt| attempt.blocked_until > now)
    {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "尝试次数过多，请稍后再试",
        ));
    }
    Ok(())
}

async fn record_auth_failure(state: &AppState, key: &str) {
    let now = now_ts();
    let mut attempts = state.auth_attempts.write().await;
    let attempt = attempts.entry(key.to_string()).or_default();
    if attempt.window_started_at == 0 || now - attempt.window_started_at > AUTH_WINDOW_SECONDS {
        attempt.failures = 0;
        attempt.window_started_at = now;
        attempt.blocked_until = 0;
    }
    attempt.failures += 1;
    if attempt.failures >= AUTH_FAILURE_LIMIT {
        attempt.blocked_until = now + AUTH_BLOCK_SECONDS;
    }
}

async fn clear_auth_failures(state: &AppState, key: &str) {
    state.auth_attempts.write().await.remove(key);
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf};

    use axum::{Json, extract::State, http::StatusCode};
    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::{
        auth::{AuthCipher, totp_code_at},
        config::Cli,
        db::init_db,
    };

    async fn test_state() -> AppState {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect("postgresql://localhost/postgres")
            .await
            .expect("connect database");
        init_db(&db).await.expect("initialize database");
        AppState::new(
            db,
            Cli {
                bind: "127.0.0.1:0".parse::<SocketAddr>().expect("bind address"),
                database_url: "postgresql://localhost/postgres".to_string(),
                database_password: None,
                admin_password: "bootstrap-password".to_string(),
                auth_secret_key: None,
                auth_key_file: PathBuf::from("unused-test-auth-key"),
                secure_cookies: false,
                reset_admin_auth: false,
                confirm_reset_admin_auth: None,
                upload_dir: PathBuf::from("unused-uploads"),
                update_dir: PathBuf::from("unused-updates"),
                agent_package_max_bytes: 1024,
            },
            AuthCipher::from_key(&[9_u8; 32]).expect("create cipher"),
        )
    }

    async fn insert_user_with_device(
        state: &AppState,
        user_id: &str,
        username: &str,
        secret: &[u8],
    ) {
        sqlx::query(
            "INSERT INTO admin_users(id, username, username_normalized, enabled, created_at) VALUES($1, $2, $3, 1, $4)",
        )
        .bind(user_id)
        .bind(username)
        .bind(username.to_ascii_lowercase())
        .bind(now_ts())
        .execute(&state.db)
        .await
        .expect("insert user");
        sqlx::query(
            "INSERT INTO authenticator_devices(id, user_id, name, secret_ciphertext, created_at) VALUES($1, $2, '认证器 1', $3, $4)",
        )
        .bind(format!("device-{user_id}"))
        .bind(user_id)
        .bind(state.auth_cipher.encrypt(secret).expect("encrypt secret"))
        .bind(now_ts())
        .execute(&state.db)
        .await
        .expect("insert device");
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn totp_login_binds_session_and_disables_bootstrap_password() {
        let state = test_state().await;
        let secret = b"12345678901234567890";
        insert_user_with_device(&state, "user-1", "Admin.One", secret).await;
        let response = admin_login(
            State(state.clone()),
            Json(LoginRequest {
                username: "admin.one".to_string(),
                code: totp_code_at(secret, now_ts()),
            }),
        )
        .await
        .expect("login succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(header::SET_COOKIE)
                .expect("session cookie")
                .to_str()
                .expect("valid header")
                .contains("SameSite=Strict")
        );
        let sessions = state.sessions.read().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions.values().next().expect("session").username,
            "Admin.One"
        );
        drop(sessions);

        let error = bootstrap_start(
            State(state),
            Json(BootstrapStartRequest {
                password: "bootstrap-password".to_string(),
                username: "second-admin".to_string(),
            }),
        )
        .await
        .expect_err("bootstrap must stay disabled");
        assert_eq!(error.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn refuses_to_remove_the_last_login_path() {
        let state = test_state().await;
        insert_user_with_device(&state, "user-1", "admin-one", b"first-secret").await;
        assert!(
            ensure_other_login_path(&state.db, Some("user-1"), None)
                .await
                .is_err()
        );

        insert_user_with_device(&state, "user-2", "admin-two", b"second-secret").await;
        ensure_other_login_path(&state.db, Some("user-1"), None)
            .await
            .expect("second administrator remains available");
        ensure_other_login_path(&state.db, None, Some("device-user-1"))
            .await
            .expect("second device remains available");
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL test database"]
    async fn reset_removes_authentication_without_erasing_audit_history() {
        let state = test_state().await;
        insert_user_with_device(&state, "user-1", "admin-one", b"first-secret").await;
        reset_admin_auth(&state.db)
            .await
            .expect("reset authentication");
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_users")
            .fetch_one(&state.db)
            .await
            .expect("count users");
        let logs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM action_logs WHERE action = 'reset_admin_auth'",
        )
        .fetch_one(&state.db)
        .await
        .expect("count logs");
        assert_eq!(users, 0);
        assert_eq!(logs, 1);
    }
}
