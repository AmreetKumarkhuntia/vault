//! The demo target: a small order service used to test vault against a real
//! black box. It reads Postgres/Redis, calls two external dependencies
//! (payments, email) through env-configured base URLs, and has one
//! deliberately-async endpoint so `eventually:` has something to chew on.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use redis::AsyncCommands;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

#[derive(Clone)]
struct App {
    db: PgPool,
    redis: redis::aio::ConnectionManager,
    http: reqwest::Client,
    payments_url: String,
    email_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost:5432/vault_demo".into());
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379/15".into());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8091);

    let db = PgPoolOptions::new()
        .max_connections(8)
        .connect(&db_url)
        .await?;
    migrate(&db).await?;
    let redis_client = redis::Client::open(redis_url)?;
    let redis = redis::aio::ConnectionManager::new(redis_client).await?;

    let app = Arc::new(App {
        db,
        redis,
        http: reqwest::Client::new(),
        payments_url: std::env::var("PAYMENTS_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9095/payments".into()),
        email_url: std::env::var("EMAIL_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9095/email".into()),
    });

    let router = Router::new()
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/api/orders", post(create_order))
        .route("/api/orders/{id}", get(get_order))
        .route("/api/orders/{id}", put(update_order))
        .route("/api/orders/{id}", delete(delete_order))
        .route("/api/orders/{id}/confirm-async", post(confirm_async))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("demo-target listening on {}", listener.local_addr()?);
    axum::serve(listener, router).await?;
    Ok(())
}

async fn migrate(db: &PgPool) -> anyhow::Result<()> {
    sqlx::raw_sql(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id BIGINT PRIMARY KEY,
            email TEXT UNIQUE NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            plan TEXT NOT NULL DEFAULT 'free'
        );
        CREATE TABLE IF NOT EXISTS wallets (
            user_id BIGINT PRIMARY KEY,
            balance_cents BIGINT NOT NULL DEFAULT 0,
            currency TEXT NOT NULL DEFAULT 'USD'
        );
        CREATE TABLE IF NOT EXISTS orders (
            id BIGSERIAL PRIMARY KEY,
            user_id BIGINT NOT NULL,
            status TEXT NOT NULL,
            total_cents BIGINT NOT NULL,
            external_charge_id TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        CREATE TABLE IF NOT EXISTS audit_log (
            id BIGSERIAL PRIMARY KEY,
            entity TEXT NOT NULL,
            entity_id TEXT NOT NULL,
            action TEXT NOT NULL,
            at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        "#,
    )
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct CreateOrder {
    user_id: i64,
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[allow(dead_code)]
    sku: String,
    qty: i64,
    price_cents: i64,
}

async fn create_order(
    State(app): State<Arc<App>>,
    Json(req): Json<CreateOrder>,
) -> (StatusCode, Json<Value>) {
    let user = sqlx::query("SELECT status FROM users WHERE id = $1")
        .bind(req.user_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten();
    match user {
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "user_not_found"})),
            )
        }
        Some(row) if row.get::<String, _>(0) != "active" => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "user_inactive"})),
            )
        }
        _ => {}
    }

    let total: i64 = req.items.iter().map(|i| i.qty * i.price_cents).sum();
    let balance: Option<i64> = sqlx::query("SELECT balance_cents FROM wallets WHERE user_id = $1")
        .bind(req.user_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()
        .map(|r| r.get(0));
    if balance.unwrap_or(0) < total {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({"error": "insufficient_funds"})),
        );
    }

    let order_id: i64 = match sqlx::query(
        "INSERT INTO orders (user_id, status, total_cents) VALUES ($1, 'pending', $2) RETURNING id",
    )
    .bind(req.user_id)
    .bind(total)
    .fetch_one(&app.db)
    .await
    {
        Ok(row) => row.get(0),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        }
    };

    let charge = app
        .http
        .post(format!("{}/v1/charges", app.payments_url))
        .json(&json!({"amount_cents": total, "currency": "USD", "order_ref": order_id}))
        .send()
        .await;
    let charge_id = match charge {
        Ok(resp) if resp.status().is_success() => resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_str).map(String::from)),
        _ => {
            sqlx::query("UPDATE orders SET status = 'failed' WHERE id = $1")
                .bind(order_id)
                .execute(&app.db)
                .await
                .ok();
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "payment_failed", "id": order_id})),
            );
        }
    };

    sqlx::query("UPDATE orders SET external_charge_id = $1 WHERE id = $2")
        .bind(&charge_id)
        .bind(order_id)
        .execute(&app.db)
        .await
        .ok();
    sqlx::query("UPDATE wallets SET balance_cents = balance_cents - $1 WHERE user_id = $2")
        .bind(total)
        .bind(req.user_id)
        .execute(&app.db)
        .await
        .ok();

    let mut r = app.redis.clone();
    let summary = json!({"status": "pending", "total_cents": total, "user_id": req.user_id});
    let _: Result<(), _> = r
        .set_ex(
            format!("order:{order_id}:summary"),
            summary.to_string(),
            3600,
        )
        .await;

    let email = app.email_url.clone();
    let http = app.http.clone();
    tokio::spawn(async move {
        let _ = http
            .post(format!("{email}/send"))
            .json(&json!({"template": "order_confirmation", "order_ref": order_id}))
            .send()
            .await;
    });

    (
        StatusCode::CREATED,
        Json(json!({
            "id": order_id,
            "user_id": req.user_id,
            "status": "pending",
            "total_cents": total,
        })),
    )
}

async fn get_order(State(app): State<Arc<App>>, Path(id): Path<i64>) -> (StatusCode, Json<Value>) {
    let row = sqlx::query(
        "SELECT id, user_id, status, total_cents, external_charge_id FROM orders WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&app.db)
    .await
    .ok()
    .flatten();
    match row {
        Some(r) => (
            StatusCode::OK,
            Json(json!({
                "id": r.get::<i64, _>(0),
                "user_id": r.get::<i64, _>(1),
                "status": r.get::<String, _>(2),
                "total_cents": r.get::<i64, _>(3),
                "external_charge_id": r.get::<Option<String>, _>(4),
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "order_not_found"})),
        ),
    }
}

#[derive(Deserialize)]
struct UpdateOrder {
    status: String,
}

async fn update_order(
    State(app): State<Arc<App>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateOrder>,
) -> (StatusCode, Json<Value>) {
    let updated = sqlx::query("UPDATE orders SET status = $1 WHERE id = $2")
        .bind(&req.status)
        .bind(id)
        .execute(&app.db)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    if updated == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "order_not_found"})),
        );
    }
    sqlx::query("INSERT INTO audit_log (entity, entity_id, action) VALUES ('order', $1, $2)")
        .bind(id.to_string())
        .bind(format!("status:{}", req.status))
        .execute(&app.db)
        .await
        .ok();
    let mut r = app.redis.clone();
    let _: Result<(), _> = r.del(format!("order:{id}:summary")).await;
    (
        StatusCode::OK,
        Json(json!({"id": id, "status": req.status})),
    )
}

async fn delete_order(State(app): State<Arc<App>>, Path(id): Path<i64>) -> StatusCode {
    let refund = app
        .http
        .post(format!("{}/v1/refunds", app.payments_url))
        .json(&json!({"order_ref": id}))
        .send()
        .await;
    if refund.map(|r| !r.status().is_success()).unwrap_or(true) {
        return StatusCode::BAD_GATEWAY;
    }
    let deleted = sqlx::query("DELETE FROM orders WHERE id = $1")
        .bind(id)
        .execute(&app.db)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0);
    if deleted == 0 {
        return StatusCode::NOT_FOUND;
    }
    sqlx::query(
        "INSERT INTO audit_log (entity, entity_id, action) VALUES ('order', $1, 'deleted')",
    )
    .bind(id.to_string())
    .execute(&app.db)
    .await
    .ok();
    let mut r = app.redis.clone();
    let _: Result<(), _> = r.del(format!("order:{id}:summary")).await;
    StatusCode::NO_CONTENT
}

/// Responds 202 immediately, then confirms the order ~300ms later — the
/// pattern `eventually:` + settle semantics exist for.
async fn confirm_async(State(app): State<Arc<App>>, Path(id): Path<i64>) -> StatusCode {
    let exists: bool = sqlx::query("SELECT 1 FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(&app.db)
        .await
        .map(|r| r.is_some())
        .unwrap_or(false);
    if !exists {
        return StatusCode::NOT_FOUND;
    }
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        sqlx::query("UPDATE orders SET status = 'confirmed' WHERE id = $1")
            .bind(id)
            .execute(&app.db)
            .await
            .ok();
        let _ = app
            .http
            .post(format!("{}/send", app.email_url))
            .json(&json!({"template": "order_confirmed", "order_ref": id}))
            .send()
            .await;
    });
    StatusCode::ACCEPTED
}
