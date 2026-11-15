//! The HTTP API and the join page.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::Deserialize;

use crate::protocol::*;
use crate::state::{self, Store};

/// Largest request body accepted. A hundred songs with long titles fit.
const BODY_LIMIT: usize = 128 * 1024;

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/", get(landing))
        .route("/jam/{code}", get(join_page))
        .route("/api/v1/info", get(info))
        .route("/api/v1/sessions", post(create))
        .route("/api/v1/sessions/{code}", get(poll))
        .route("/api/v1/sessions/{code}/join", post(join))
        .route("/api/v1/sessions/{code}/leave", post(leave))
        .route("/api/v1/sessions/{code}/playback", put(publish))
        .route("/api/v1/sessions/{code}/queue", post(add))
        .route("/api/v1/sessions/{code}/queue/remove", post(remove))
        .route("/api/v1/sessions/{code}/queue/move", post(reorder))
        .route("/api/v1/sessions/{code}/commands", post(command))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(BODY_LIMIT))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(store)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

pub struct ApiError(state::Error);

impl From<state::Error> for ApiError {
    fn from(error: state::Error) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        use state::Error::*;
        let (status, message) = match self.0 {
            NotFound => (StatusCode::NOT_FOUND, "no jam has this code".to_owned()),
            Ended => (StatusCode::GONE, "this jam has ended".to_owned()),
            Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "not a listener in this jam".to_owned(),
            ),
            Forbidden(why) => (StatusCode::FORBIDDEN, why.to_owned()),
            Invalid(why) => (StatusCode::BAD_REQUEST, why),
            Full(why) => (StatusCode::CONFLICT, why.to_owned()),
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(ApiError(state::Error::Unauthorized))
}

async fn info() -> Json<ServerInfo> {
    Json(ServerInfo {
        name: "spotifast-jam-server".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL,
    })
}

async fn create(
    State(store): State<Arc<Store>>,
    Json(request): Json<JoinRequest>,
) -> Result<(StatusCode, Json<JoinResponse>), ApiError> {
    let joined = store.create(&request.name, now_ms())?;
    tracing::info!(code = %joined.session.id, "jam started");
    Ok((StatusCode::CREATED, Json(joined)))
}

async fn join(
    State(store): State<Arc<Store>>,
    Path(code): Path<String>,
    Json(request): Json<JoinRequest>,
) -> ApiResult<JoinResponse> {
    Ok(Json(store.join(&code, &request.name, now_ms())?))
}

#[derive(Deserialize)]
struct PollQuery {
    #[serde(default)]
    since: u64,
    #[serde(default)]
    wait: u64,
}

/// Answers at once when the jam changed since `since`; otherwise holds the
/// request up to `wait` seconds for the next change.
async fn poll(
    State(store): State<Arc<Store>>,
    Path(code): Path<String>,
    Query(query): Query<PollQuery>,
    headers: HeaderMap,
) -> ApiResult<SessionView> {
    let token = bearer(&headers)?;
    let (view, mut changed) = store.view(&code, token, now_ms())?;
    let wait = query.wait.min(MAX_WAIT_SECS);
    if wait == 0 || view.ended || view.version > query.since {
        return Ok(Json(view));
    }
    let since = query.since;
    // A closed channel means the jam was forgotten; the view below says so.
    let _ = tokio::time::timeout(
        Duration::from_secs(wait),
        changed.wait_for(|version| *version > since),
    )
    .await;
    let (view, _) = store.view(&code, token, now_ms())?;
    Ok(Json(view))
}

async fn leave(
    State(store): State<Arc<Store>>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    store.leave(&code, bearer(&headers)?, now_ms())?;
    Ok(StatusCode::NO_CONTENT)
}

async fn publish(
    State(store): State<Arc<Store>>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(playback): Json<PublishPlayback>,
) -> ApiResult<SessionView> {
    Ok(Json(store.publish(
        &code,
        bearer(&headers)?,
        playback,
        now_ms(),
    )?))
}

async fn add(
    State(store): State<Arc<Store>>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(request): Json<AddToQueue>,
) -> ApiResult<SessionView> {
    Ok(Json(store.add(
        &code,
        bearer(&headers)?,
        request,
        now_ms(),
    )?))
}

async fn remove(
    State(store): State<Arc<Store>>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(request): Json<RemoveFromQueue>,
) -> ApiResult<SessionView> {
    Ok(Json(store.remove(
        &code,
        bearer(&headers)?,
        request,
        now_ms(),
    )?))
}

async fn reorder(
    State(store): State<Arc<Store>>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(request): Json<MoveInQueue>,
) -> ApiResult<SessionView> {
    Ok(Json(store.reorder(
        &code,
        bearer(&headers)?,
        request,
        now_ms(),
    )?))
}

async fn command(
    State(store): State<Arc<Store>>,
    Path(code): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SendCommand>,
) -> ApiResult<CommandAccepted> {
    Ok(Json(store.command(
        &code,
        bearer(&headers)?,
        request.command,
        now_ms(),
    )?))
}

async fn landing() -> Html<&'static str> {
    Html(include_str!("landing.html"))
}

/// The page an invite link opens in a browser. It hands the jam to
/// Spotifast through the `spotifast-jam:` link and explains what to do when
/// nothing answers that link.
async fn join_page(Path(code): Path<String>) -> Response {
    let Some(code) = state::normalize_code(&code) else {
        return (StatusCode::NOT_FOUND, Html(include_str!("landing.html"))).into_response();
    };
    // The code is six characters from a fixed alphabet, so it needs no
    // escaping in the page.
    Html(include_str!("join.html").replace("{{CODE}}", &code)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> Router {
        router(Arc::new(Store::new(Default::default())))
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn a_jam_round_trip_over_http() {
        let app = app();
        let (status, info) = call(&app, "GET", "/api/v1/info", None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["protocol"], PROTOCOL);

        let (status, host) = call(
            &app,
            "POST",
            "/api/v1/sessions",
            None,
            Some(serde_json::json!({ "name": "Host" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let code = host["session"]["id"].as_str().unwrap().to_owned();
        let host_token = host["token"].as_str().unwrap().to_owned();

        let (status, guest) = call(
            &app,
            "POST",
            &format!("/api/v1/sessions/{code}/join"),
            None,
            Some(serde_json::json!({ "name": "Guest" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let guest_token = guest["token"].as_str().unwrap().to_owned();

        let (status, view) = call(
            &app,
            "POST",
            &format!("/api/v1/sessions/{code}/queue"),
            Some(&guest_token),
            Some(serde_json::json!({ "tracks": [{ "uri": "spotify:track:abc", "title": "A" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["queue"][0]["track"]["title"], "A");

        let (status, accepted) = call(
            &app,
            "POST",
            &format!("/api/v1/sessions/{code}/commands"),
            Some(&guest_token),
            Some(serde_json::json!({ "command": { "type": "seek", "position_ms": 5000 } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(accepted["seq"], 1);

        let (status, _) = call(
            &app,
            "PUT",
            &format!("/api/v1/sessions/{code}/playback"),
            Some(&guest_token),
            Some(serde_json::json!({ "track": null, "position_ms": 0, "playing": false })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, view) = call(
            &app,
            "PUT",
            &format!("/api/v1/sessions/{code}/playback"),
            Some(&host_token),
            Some(serde_json::json!({
                "track": { "uri": "spotify:track:abc" },
                "position_ms": 5000,
                "playing": true,
                "applied_seq": 1
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["playback"]["applied_seq"], 1);

        // Already newer than `since`: answers without waiting.
        let (status, view) = call(
            &app,
            "GET",
            &format!("/api/v1/sessions/{code}?since=0&wait=30"),
            Some(&guest_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["playback"]["playing"], true);

        let (status, _) = call(&app, "GET", &format!("/api/v1/sessions/{code}"), None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = call(
            &app,
            "POST",
            &format!("/api/v1/sessions/{code}/leave"),
            Some(&host_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(
            &app,
            "POST",
            &format!("/api/v1/sessions/{code}/join"),
            None,
            Some(serde_json::json!({ "name": "Late" })),
        )
        .await;
        assert_eq!(status, StatusCode::GONE);
    }

    #[tokio::test]
    async fn an_unknown_code_is_not_found() {
        let app = app();
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/sessions/ZZZZZZ/join",
            None,
            Some(serde_json::json!({ "name": "Guest" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn the_join_page_carries_the_code_and_the_app_link() {
        let app = app();
        let response = app
            .clone()
            .oneshot(Request::get("/jam/abcdef").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let html = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("ABCDEF"));
        assert!(html.contains("spotifast-jam://join"));
        assert!(!html.contains("{{CODE}}"));
    }
}
