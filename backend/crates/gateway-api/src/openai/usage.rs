//! Bearer Key 的只读额度查询，与自助页面共用当前预算账本。

use std::time::SystemTime;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use gateway_core::metering::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiState, auth::SessionState as _};

use super::{
    auth::{bearer_client_api_key, client_access_error_response},
    error::{missing_client_api_key_response, openai_error_response},
};

pub(super) fn router() -> Router<ApiState> {
    Router::new()
        .route("/v1/usage", get(usage))
        .layer(middleware::map_response(no_store))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageQuery {}

async fn usage(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    let key = match bearer_client_api_key(&headers) {
        Ok(key) => key,
        Err(error) => return client_access_error_response(error.into()),
    };
    // 范围只能来自 Bearer 身份，不接受调用者指定 Key、账号或时间窗口。
    if Query::<UsageQuery>::try_from_uri(&uri).is_err() {
        return openai_error_response(
            StatusCode::BAD_REQUEST,
            "Usage query does not accept query parameters",
            "invalid_request_error",
            "invalid_usage_query",
        )
        .into_response();
    }
    let budget = match state.admin_services().key_usage().budget(key).await {
        Ok(Some(budget)) => budget,
        Ok(None) => return missing_client_api_key_response().into_response(),
        Err(_) => {
            return openai_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Key usage is temporarily unavailable",
                "server_error",
                "usage_unavailable",
            )
            .into_response();
        }
    };
    Json(json!({
        "unit": "USD",
        "daily": window(budget.limits.daily_usd, budget.daily_used_usd, budget.daily_resets_at),
        "weekly": window(budget.limits.weekly_usd, budget.weekly_used_usd, budget.weekly_resets_at),
    }))
    .into_response()
}

fn window(total: Decimal, used: Decimal, resets_at: Option<SystemTime>) -> Value {
    let limited = total != Decimal::ZERO;
    json!({
        "total": limited.then(|| total.canonical()),
        "used": used.canonical(),
        "remaining": limited.then(|| total.saturating_sub(used).canonical()),
        "resetsAt": resets_at.map(DateTime::<Utc>::from),
    })
}

async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
