use crate::{api::auth::internal, state::AppState, unifi};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};
use std::sync::Arc;
type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

pub async fn status(State(state): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(
        json!({"connection":state.unifi.snapshot.read().await.clone(),
        "clients":unifi::clients(&state.db).await.map_err(internal)?,
        "events":unifi::events(&state.db).await.map_err(internal)?}),
    ))
}
pub async fn sync(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    state.unifi.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({"message":"Sync queued"})))
}
pub async fn binding(
    State(state): State<Arc<AppState>>,
    Path(mac): Path<String>,
    Json(body): Json<unifi::Binding>,
) -> ApiResult {
    let bad = |e: anyhow::Error| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":e.to_string()})),
        )
    };
    let mac = unifi::normalize_mac(&mac).map_err(bad)?;
    let _guard = state.unifi.lock.try_lock().map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":"UniFi synchronisatie bezig; probeer het zo opnieuw"})),
        )
    })?;
    unifi::bind(&state.db, &mac, &body).await.map_err(bad)?;
    state.unifi.wake.notify_one();
    Ok(Json(
        json!({"message":"Binding saved; application pending"}),
    ))
}
