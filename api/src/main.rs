mod image_io;
mod protection;
#[allow(dead_code, clippy::all)]
mod upstream;

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use http_body_util::BodyExt;
use protection::{Models, Options, ALGORITHMS};
use serde_json::json;
use std::{
    net::SocketAddr,
    path::Path,
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use upstream::types::{ProtectionProgress, ProtectionState};

const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

struct AppState {
    models: Mutex<Models>,
    slot: Arc<Semaphore>,
    selected_algorithm: Option<String>,
    progress: Mutex<Option<ProgressSnapshot>>,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProgressSnapshot {
    request_id: String,
    stage: String,
    tile_current: u32,
    tile_total: u32,
    iteration_current: u32,
    iteration_total: u32,
    completed_iterations: u64,
    total_iterations: u64,
    elapsed_ms: u64,
    estimated_remaining_ms: Option<u64>,
    #[serde(skip_serializing)]
    started_at: Instant,
}

impl ProgressSnapshot {
    fn from_protection(
        request_id: &str,
        progress: ProtectionProgress,
        started_at: Instant,
    ) -> Self {
        let total_iterations = u64::from(progress.tile_total) * u64::from(progress.iteration_total);
        let completed_iterations = u64::from(progress.tile_current.saturating_sub(1))
            * u64::from(progress.iteration_total)
            + u64::from(progress.iteration_current);
        let mut snapshot = Self {
            request_id: request_id.to_string(),
            stage: progress.stage,
            tile_current: progress.tile_current,
            tile_total: progress.tile_total,
            iteration_current: progress.iteration_current,
            iteration_total: progress.iteration_total,
            completed_iterations,
            total_iterations,
            elapsed_ms: 0,
            estimated_remaining_ms: None,
            started_at,
        };
        snapshot.refresh_timing(Instant::now());
        snapshot
    }

    fn refresh_timing(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.started_at);
        self.elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        self.estimated_remaining_ms = if self.stage == "processing"
            && elapsed >= Duration::from_secs(5)
            && self.completed_iterations > 0
        {
            let remaining = self
                .total_iterations
                .saturating_sub(self.completed_iterations);
            let estimated_ms = elapsed
                .as_millis()
                .saturating_mul(u128::from(remaining))
                / u128::from(self.completed_iterations);
            Some(u64::try_from(estimated_ms).unwrap_or(u64::MAX))
        } else {
            None
        };
    }
}

struct ProgressRegistration {
    state: Arc<AppState>,
    request_id: String,
}

impl ProgressRegistration {
    fn new(state: Arc<AppState>, request_id: String) -> Result<Self, &'static str> {
        *state
            .progress
            .lock()
            .map_err(|_| "Progress unavailable")? = None;
        Ok(Self {
            state,
            request_id,
        })
    }

    fn publish(&self, progress: ProtectionProgress, started_at: Instant) {
        if let Ok(mut snapshot) = self.state.progress.lock() {
            *snapshot = Some(ProgressSnapshot::from_protection(
                &self.request_id,
                progress,
                started_at,
            ));
        }
    }

    fn mark_encoding(&self) {
        if let Ok(mut current) = self.state.progress.lock() {
            if let Some(snapshot) = current
                .as_mut()
                .filter(|snapshot| snapshot.request_id == self.request_id)
            {
                snapshot.stage = "encoding".to_string();
                snapshot.refresh_timing(Instant::now());
            }
        }
    }

    fn current(&self) -> Option<ProgressSnapshot> {
        self.state
            .progress
            .lock()
            .ok()
            .and_then(|snapshot| {
                snapshot
                    .as_ref()
                    .filter(|snapshot| snapshot.request_id == self.request_id)
                    .cloned()
            })
    }
}

impl Drop for ProgressRegistration {
    fn drop(&mut self) {
        if let Ok(mut snapshot) = self.state.progress.lock() {
            if snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.request_id == self.request_id)
            {
                *snapshot = None;
            }
        }
    }
}

struct RequestCancellation(Option<Arc<ProtectionState>>);

impl RequestCancellation {
    fn new(state: Arc<ProtectionState>) -> Self {
        Self(Some(state))
    }

    fn complete(&mut self) {
        self.0 = None;
    }
}

impl Drop for RequestCancellation {
    fn drop(&mut self) {
        if let Some(state) = &self.0 {
            state.is_cancelled.store(true, Ordering::SeqCst);
        }
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"error": message.into()})),
    )
        .into_response()
}

fn parse_request_id(headers: &HeaderMap, required: bool) -> Result<Option<String>, &'static str> {
    let mut values = headers.get_all("x-hope-request-id").iter();
    let Some(value) = values.next() else {
        return if required {
            Err("X-Hope-Request-Id is required")
        } else {
            Ok(None)
        };
    };
    if values.next().is_some() {
        return Err("X-Hope-Request-Id must appear once");
    }
    let value = value
        .to_str()
        .map_err(|_| "X-Hope-Request-Id is invalid")?;
    if !(1..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err("X-Hope-Request-Id is invalid");
    }
    Ok(Some(value.to_string()))
}

async fn reject(request: Request, status: StatusCode, message: impl Into<String>) -> Response {
    let mut body = request.into_body();
    let _ = tokio::time::timeout(Duration::from_secs(30), async {
        let mut received = 0;
        while let Some(Ok(frame)) = body.frame().await {
            if let Some(data) = frame.data_ref() {
                received += data.len();
                if received > MAX_BODY_BYTES {
                    break;
                }
            }
        }
    })
    .await;
    error(status, message)
}

fn selected_algorithm(value: Result<String, std::env::VarError>) -> Result<Option<String>, String> {
    match value {
        Ok(algorithm) if ALGORITHMS.contains(&algorithm.as_str()) => Ok(Some(algorithm)),
        Ok(_) => Err("HOPE_API_ALGORITHM must be noise, glaze, or nightshade".into()),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("Invalid HOPE_API_ALGORITHM: {error}")),
    }
}

fn listen_address(value: Result<String, std::env::VarError>) -> Result<SocketAddr, String> {
    let address: SocketAddr = value
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
        .map_err(|_| "Invalid HOPE_API_ADDR")?;
    if !address.ip().is_loopback() {
        return Err("HOPE_API_ADDR must use a loopback address".into());
    }
    Ok(address)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let algorithms = state
        .selected_algorithm
        .as_ref()
        .map_or_else(|| json!(ALGORITHMS), |algorithm| json!([algorithm]));
    Json(json!({"ready": true, "provider": "cpu", "algorithms": algorithms}))
}

async fn progress(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let request_id = match parse_request_id(request.headers(), true) {
        Ok(Some(request_id)) => request_id,
        Ok(None) => return error(StatusCode::BAD_REQUEST, "X-Hope-Request-Id is required"),
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let progress = state.progress.lock().ok().and_then(|snapshot| {
        snapshot
            .as_ref()
            .filter(|snapshot| snapshot.request_id == request_id)
            .cloned()
    });
    let progress = progress.map(|mut snapshot| {
        snapshot.refresh_timing(Instant::now());
        snapshot
    });
    let mut response = Json(json!({"progress": progress})).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn protect(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let started = Instant::now();
    let request_id = match parse_request_id(request.headers(), false) {
        Ok(request_id) => request_id,
        Err(message) => return reject(request, StatusCode::BAD_REQUEST, message).await,
    };
    let options = match axum::extract::Query::<Options>::try_from_uri(request.uri()) {
        Ok(options) => options.0,
        Err(_) => {
            return reject(
                request,
                StatusCode::BAD_REQUEST,
                "Invalid query; algorithm and iterations are required",
            )
            .await
        }
    };
    if state
        .selected_algorithm
        .as_deref()
        .is_some_and(|algorithm| algorithm != options.algorithm)
    {
        return reject(
            request,
            StatusCode::BAD_REQUEST,
            "Requested algorithm is not enabled",
        )
        .await;
    }
    if let Err(message) = options.validate() {
        return reject(request, StatusCode::BAD_REQUEST, message).await;
    }
    let permit = match state.slot.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return reject(
                request,
                StatusCode::SERVICE_UNAVAILABLE,
                "Worker busy; retry later",
            )
            .await
        }
    };
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|size| size > MAX_BODY_BYTES as u64)
    {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "Input exceeds 32 MiB");
    }
    let bytes = match tokio::time::timeout(
        Duration::from_secs(30),
        to_bytes(request.into_body(), MAX_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Body unreadable or exceeds 32 MiB",
            )
        }
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "Upload timed out"),
    };
    let progress_registration = match request_id {
        Some(request_id) => match ProgressRegistration::new(state.clone(), request_id) {
            Ok(registration) => Some(registration),
            Err(message) => return error(StatusCode::INTERNAL_SERVER_ERROR, message),
        },
        None => None,
    };
    let protection_state = Arc::new(ProtectionState::default());
    let mut cancellation = RequestCancellation::new(protection_state.clone());
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let progress_registration = progress_registration;
        let image = image_io::decode(&bytes, options.max_edge)
            .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))?;
        let mut models = state.models.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Worker unavailable".to_string(),
            )
        })?;
        let protection_started = Instant::now();
        let (image, calls) = models
            .protect_with_state_and_progress(
                &image,
                &options,
                &protection_state,
                &|progress| {
                    if let Some(registration) = &progress_registration {
                        registration.publish(progress, protection_started);
                    }
                },
            )
            .map_err(|message| {
                eprintln!("Protection failed: {message}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Protection failed; no image returned".to_string(),
                )
            })?;
        if let Some(registration) = &progress_registration {
            registration.mark_encoding();
        }
        let encoded = image_io::encode(&image, options.quality)
            .map_err(|message| (StatusCode::INTERNAL_SERVER_ERROR, message))?;
        Ok::<_, (StatusCode, String)>((encoded, calls, image.width(), image.height()))
    })
    .await;
    cancellation.complete();
    match result {
        Ok(Ok((encoded, calls, width, height))) => {
            let mut response = (
                [
                    (header::CONTENT_TYPE, "image/webp"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                encoded,
            )
                .into_response();
            for (name, value) in [
                ("x-hope-model-calls", calls.to_string()),
                ("x-hope-width", width.to_string()),
                ("x-hope-height", height.to_string()),
                (
                    "x-hope-total-ms",
                    format!("{:.2}", started.elapsed().as_secs_f64() * 1000.0),
                ),
            ] {
                if let Ok(value) = HeaderValue::from_str(&value) {
                    response.headers_mut().insert(name, value);
                }
            }
            response
        }
        Ok(Err((status, message))) => error(status, message),
        Err(message) => {
            eprintln!("Worker failed: {message}");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Worker failed; no image returned",
            )
        }
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/progress", get(progress))
        .route("/protect", post(protect))
        .with_state(state)
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let selected_algorithm = selected_algorithm(std::env::var("HOPE_API_ALGORITHM"))?;
    let address = listen_address(std::env::var("HOPE_API_ADDR"))?;
    let directory = std::env::var("HOPE_MODEL_DIR")
        .map_err(|_| "Set HOPE_MODEL_DIR to the directory containing all three model weights")?;
    let model_dir = Path::new(&directory);
    let models = match selected_algorithm.as_deref() {
        Some(algorithm) => Models::load_for(model_dir, algorithm)?,
        None => Models::load(model_dir)?,
    };
    let state = Arc::new(AppState {
        models: Mutex::new(models),
        slot: Arc::new(Semaphore::new(1)),
        selected_algorithm,
        progress: Mutex::new(None),
    });
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| error.to_string())?;
    eprintln!("Hope API listening on {address}; CPU, one job at a time; loopback only");
    axum::serve(listener, router(state))
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest};
    use std::{
        io::Write,
        net::TcpStream,
        sync::atomic::{AtomicBool, AtomicUsize},
        thread,
    };
    use tower::ServiceExt;

    #[derive(Default)]
    struct DisconnectTestWork {
        started: AtomicUsize,
        release: AtomicBool,
        cancelled: AtomicBool,
        finished: AtomicBool,
    }

    struct DisconnectTestState {
        slot: Arc<Semaphore>,
        work: Arc<DisconnectTestWork>,
        cancellation: Arc<ProtectionState>,
    }

    struct ReleaseOnDrop(Arc<DisconnectTestWork>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release.store(true, Ordering::SeqCst);
        }
    }

    async fn disconnect_test_worker(State(state): State<Arc<DisconnectTestState>>) -> StatusCode {
        let permit = match state.slot.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
        };
        let mut cancellation = RequestCancellation::new(state.cancellation.clone());
        let work = state.work.clone();
        let cancelled = state.cancellation.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work.started.fetch_add(1, Ordering::SeqCst);
            while !work.release.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            if cancelled.is_cancelled.load(Ordering::SeqCst) {
                work.cancelled.store(true, Ordering::SeqCst);
            }
            work.finished.store(true, Ordering::SeqCst);
        })
        .await;
        cancellation.complete();
        if result.is_err() {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::OK
        }
    }

    fn state() -> Arc<AppState> {
        Arc::new(AppState {
            models: Mutex::new(Models::empty_for_test()),
            slot: Arc::new(Semaphore::new(1)),
            selected_algorithm: None,
            progress: Mutex::new(None),
        })
    }

    async fn get_progress(app: &Router, request_id: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/progress")
                    .header("x-hope-request-id", request_id)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn progress_route_requires_a_single_valid_request_id_and_returns_null_for_unknown_ids() {
        let app = router(state());
        let missing = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/progress")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let too_long = "x".repeat(129);
        for request_id in ["", "invalid/id", too_long.as_str()] {
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/progress")
                        .header("x-hope-request-id", request_id)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        let mut duplicate = HttpRequest::builder()
            .uri("/progress")
            .body(Body::empty())
            .unwrap();
        duplicate
            .headers_mut()
            .append("x-hope-request-id", HeaderValue::from_static("first"));
        duplicate
            .headers_mut()
            .append("x-hope-request-id", HeaderValue::from_static("second"));
        let response = app.clone().oneshot(duplicate).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let invalid_post = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=noise&iterations=1")
                    .header("x-hope-request-id", "invalid/id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_post.status(), StatusCode::BAD_REQUEST);

        let mut duplicate_post = HttpRequest::builder()
            .method("POST")
            .uri("/protect?algorithm=noise&iterations=1")
            .body(Body::empty())
            .unwrap();
        duplicate_post
            .headers_mut()
            .append("x-hope-request-id", HeaderValue::from_static("first"));
        duplicate_post
            .headers_mut()
            .append("x-hope-request-id", HeaderValue::from_static("second"));
        let response = app.clone().oneshot(duplicate_post).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let (status, body) = get_progress(&app, "unknown-id").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["progress"], serde_json::Value::Null);
        let max_id = "a".repeat(128);
        let (status, body) = get_progress(&app, &max_id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["progress"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn progress_route_reports_iteration_counts_and_clears_the_active_snapshot() {
        let state = state();
        let app = router(state.clone());
        let registration =
            ProgressRegistration::new(state, "progress-test-1".to_string()).unwrap();

        let (status, body) = get_progress(&app, "progress-test-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["progress"], serde_json::Value::Null);

        let upstream_progress = ProtectionProgress {
            stage: "processing".to_string(),
            tile_current: 3,
            tile_total: 4,
            iteration_current: 2,
            iteration_total: 10,
            percent: 95.0,
        };
        registration.publish(upstream_progress.clone(), Instant::now());
        let (_, body) = get_progress(&app, "progress-test-1").await;
        assert_eq!(body["progress"]["estimatedRemainingMs"], serde_json::Value::Null);

        registration.publish(
            upstream_progress,
            Instant::now() - Duration::from_secs(6),
        );
        let (_, body) = get_progress(&app, "progress-test-1").await;
        let snapshot = &body["progress"];
        assert_eq!(snapshot["requestId"], "progress-test-1");
        assert_eq!(snapshot["stage"], "processing");
        assert_eq!(snapshot["completedIterations"], 22);
        assert_eq!(snapshot["totalIterations"], 40);
        let elapsed_ms = snapshot["elapsedMs"].as_u64().unwrap();
        let estimated_remaining_ms = snapshot["estimatedRemainingMs"].as_u64().unwrap();
        let expected_remaining_ms = elapsed_ms * 18 / 22;
        assert!(estimated_remaining_ms.abs_diff(expected_remaining_ms) < 10);

        registration.mark_encoding();
        let (_, body) = get_progress(&app, "progress-test-1").await;
        assert_eq!(body["progress"]["stage"], "encoding");
        assert_eq!(body["progress"]["estimatedRemainingMs"], serde_json::Value::Null);

        drop(registration);
        let (_, body) = get_progress(&app, "progress-test-1").await;
        assert_eq!(body["progress"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn protect_clears_request_progress_after_decode_failure() {
        let app = router(state());
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=noise&iterations=1")
                    .header("x-hope-request-id", "decode-failure")
                    .body(Body::from("not an image"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let (status, body) = get_progress(&app, "decode-failure").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["progress"], serde_json::Value::Null);
    }

    #[test]
    fn selected_algorithm_defaults_to_all_and_rejects_invalid_values() {
        assert_eq!(
            selected_algorithm(Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        assert_eq!(
            selected_algorithm(Ok("nightshade".into())).unwrap(),
            Some("nightshade".into())
        );
        assert!(selected_algorithm(Ok("unknown".into())).is_err());
    }

    #[test]
    fn listen_address_accepts_ipv4_and_ipv6_loopback() {
        assert_eq!(
            listen_address(Err(std::env::VarError::NotPresent)).unwrap(),
            "127.0.0.1:8787".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            listen_address(Ok("127.0.0.2:8787".into())).unwrap(),
            "127.0.0.2:8787".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            listen_address(Ok("[::1]:8787".into())).unwrap(),
            "[::1]:8787".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn listen_address_rejects_non_loopback_addresses() {
        for address in [
            "0.0.0.0:8787",
            "[::]:8787",
            "192.168.1.20:8787",
            "100.64.0.2:8787",
        ] {
            assert!(listen_address(Ok(address.into())).is_err(), "{address}");
        }
    }

    #[tokio::test]
    async fn accepts_requests_without_authorization_and_rejects_invalid_oversized_and_busy_requests(
    ) {
        let state = state();
        let app = router(state.clone());
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        for (uri, body, expected) in [
            ("/protect?algorithm=noise", "", StatusCode::BAD_REQUEST),
            (
                "/protect?algorithm=noise&iterations=1&unknown=1",
                "",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/protect?algorithm=noise&iterations=1",
                "not an image",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .method("POST")
                        .uri(uri)
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=noise&iterations=1")
                    .header(header::CONTENT_LENGTH, MAX_BODY_BYTES + 1)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let _permit = state.slot.clone().try_acquire_owned().unwrap();
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=noise&iterations=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn selected_algorithm_is_reported_and_enforced() {
        let default = router(state());
        let response = default
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["algorithms"], json!(ALGORITHMS));

        let mut selected_state = state();
        Arc::get_mut(&mut selected_state)
            .unwrap()
            .selected_algorithm = Some("nightshade".into());
        let app = router(selected_state);
        let health = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(health.into_body(), MAX_BODY_BYTES).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["algorithms"], json!(["nightshade"]));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=noise&iterations=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[ignore = "requires official weights in HOPE_MODEL_DIR"]
    async fn real_models_handle_repeated_http_requests() {
        let directory = std::env::var("HOPE_MODEL_DIR").expect("set HOPE_MODEL_DIR");
        let state = Arc::new(AppState {
            models: Mutex::new(Models::load(Path::new(&directory)).unwrap()),
            slot: Arc::new(Semaphore::new(1)),
            selected_algorithm: None,
            progress: Mutex::new(None),
        });
        let app = router(state.clone());
        let image =
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(256, 256, |column, row| {
                image::Rgba([column as u8, row as u8, ((column + row) % 256) as u8, 200])
            }));
        let input = image_io::encode(&image, 90.0).unwrap();
        for algorithm in ALGORITHMS {
            for _round in 0..2 {
                let response = app
                    .clone()
                    .oneshot(
                        HttpRequest::builder()
                            .method("POST")
                            .uri(format!(
                                "/protect?algorithm={algorithm}&iterations=1&max_edge=128"
                            ))
                            .body(Body::from(input.clone()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()[header::CONTENT_TYPE], "image/webp");
                assert_eq!(response.headers()["x-hope-model-calls"], "8");
                let webp = to_bytes(response.into_body(), MAX_BODY_BYTES)
                    .await
                    .unwrap();
                let output = image_io::decode(&webp, 4096).unwrap();
                assert_eq!((output.width(), output.height()), (128, 128));
                assert!(output.to_rgba8().pixels().all(|pixel| pixel[3] == 200));
            }
        }
        let _permit = state.slot.clone().try_acquire_owned().unwrap();
        let health = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }

    #[tokio::test]
    #[ignore = "requires official weights in HOPE_MODEL_DIR"]
    async fn real_nightshade_http_resizes_and_returns_protected_webp() {
        let directory = std::env::var("HOPE_MODEL_DIR").expect("set HOPE_MODEL_DIR");
        let state = Arc::new(AppState {
            models: Mutex::new(Models::load_for(Path::new(&directory), "nightshade").unwrap()),
            slot: Arc::new(Semaphore::new(1)),
            selected_algorithm: Some("nightshade".into()),
            progress: Mutex::new(None),
        });
        let app = router(state.clone());
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(
            1280,
            1280,
            |column, row| image::Rgba([column as u8, row as u8, ((column + row) % 256) as u8, 200]),
        ));
        let input = image_io::encode(&image, 90.0).unwrap();
        let baseline = image_io::decode(&input, 1024).unwrap();
        let baseline = image_io::encode(&baseline, 80.0).unwrap();
        let baseline = image_io::decode(&baseline, 4096).unwrap().to_rgba8();
        let request_id = "nightshade-progress-test";
        let protection_app = app.clone();
        let protection = tokio::spawn(async move {
            protection_app
                .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=nightshade&iterations=2&max_edge=1024")
                    .header("x-hope-request-id", request_id)
                    .body(Body::from(input))
                    .unwrap(),
                )
                .await
        });
        let mut progress_seen = false;
        while !protection.is_finished() {
            let (status, body) = get_progress(&app, request_id).await;
            assert_eq!(status, StatusCode::OK);
            if let Some(snapshot) = body["progress"].as_object() {
                assert_eq!(snapshot["requestId"], request_id);
                assert!(snapshot["stage"] == "processing" || snapshot["stage"] == "encoding");
                assert!(snapshot["completedIterations"].as_u64().unwrap()
                    <= snapshot["totalIterations"].as_u64().unwrap());
                progress_seen = true;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let response = protection.await.unwrap().unwrap();
        assert!(progress_seen);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/webp");
        assert_eq!(response.headers()["x-hope-model-calls"], "576");
        assert_eq!(response.headers()["x-hope-width"], "1024");
        assert_eq!(response.headers()["x-hope-height"], "1024");
        let webp = to_bytes(response.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap();
        let output = image_io::decode(&webp, 4096).unwrap().to_rgba8();
        assert_eq!((output.width(), output.height()), (1024, 1024));
        assert!(output.pixels().all(|pixel| pixel[3] == 200));
        assert!(output
            .pixels()
            .zip(baseline.pixels())
            .any(|(output, baseline)| {
                output[0] != baseline[0] || output[1] != baseline[1] || output[2] != baseline[2]
            }));
        let (_, body) = get_progress(&app, request_id).await;
        assert_eq!(body["progress"], serde_json::Value::Null);
        let _permit = state.slot.clone().try_acquire_owned().unwrap();
        let health = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn early_rejections_drain_uploads_without_buffering_them() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let state = state();
        let _permit = state.slot.clone().try_acquire_owned().unwrap();
        let app = router(state);
        for (uri, status) in [
            ("/protect?algorithm=noise", StatusCode::BAD_REQUEST),
            (
                "/protect?algorithm=noise&iterations=1",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let consumed = Arc::new(AtomicUsize::new(0));
            let observed = consumed.clone();
            let body = Body::from(vec![42; 512 * 1024]).map_frame(move |frame| {
                observed.fetch_add(
                    frame.data_ref().map_or(0, |data| data.len()),
                    Ordering::Relaxed,
                );
                frame
            });
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .method("POST")
                        .uri(uri)
                        .body(Body::new(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(consumed.load(Ordering::Relaxed), 512 * 1024);
        }
    }

    #[tokio::test]
    #[ignore = "requires official weights in HOPE_MODEL_DIR"]
    async fn real_model_tcp_disconnect_cancels_protection_and_releases_the_worker() {
        let directory = std::env::var("HOPE_MODEL_DIR").expect("set HOPE_MODEL_DIR");
        let state = Arc::new(AppState {
            models: Mutex::new(Models::load_for(Path::new(&directory), "glaze").unwrap()),
            slot: Arc::new(Semaphore::new(1)),
            selected_algorithm: Some("glaze".into()),
            progress: Mutex::new(None),
        });
        let app = router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, shutdown_signal) = tokio::sync::oneshot::channel();
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, server_app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_signal.await;
                })
                .await
        });
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(
            224,
            224,
            |column, row| image::Rgba([column as u8, row as u8, ((column + row) % 256) as u8, 200]),
        ));
        let input = image_io::encode(&image, 90.0).unwrap();
        let request_id = "tcp-disconnect-progress";
        let mut client = TcpStream::connect(address).unwrap();
        write!(
            client,
            "POST /protect?algorithm=glaze&iterations=100&max_edge=224 HTTP/1.1\r\nHost: localhost\r\nX-Hope-Request-Id: {request_id}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            input.len()
        )
        .unwrap();
        client.write_all(&input).unwrap();

        let first = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let (status, body) = get_progress(&app, request_id).await;
                assert_eq!(status, StatusCode::OK);
                if let Some(snapshot) = body["progress"].as_object() {
                    break serde_json::Value::Object(snapshot.clone());
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(first["stage"], "processing");
        assert_eq!(first["tileCurrent"], 1);
        assert_eq!(first["tileTotal"], 1);
        let first_completed = first["completedIterations"].as_u64().unwrap();
        let total_iterations = first["totalIterations"].as_u64().unwrap();
        assert!(first_completed > 0 && first_completed <= 10);
        assert_eq!(total_iterations, 100);

        let busy = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=glaze&iterations=100&max_edge=224")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(client);
        let mut max_completed = first_completed;
        let cleaned_up = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let (status, body) = get_progress(&app, request_id).await;
                assert_eq!(status, StatusCode::OK);
                if let Some(snapshot) = body["progress"].as_object() {
                    max_completed = max_completed.max(
                        snapshot["completedIterations"].as_u64().unwrap(),
                    );
                } else if state.slot.available_permits() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .is_ok();
        if !cleaned_up {
            let _ = tokio::time::timeout(Duration::from_secs(120), async {
                while state.slot.available_permits() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
        }
        let _ = shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
        assert!(cleaned_up, "disconnected protection did not finish in time");
        assert!(max_completed < total_iterations);
        assert!(max_completed <= first_completed + 10);
        assert_eq!(state.slot.available_permits(), 1);

        let followup = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=glaze&iterations=100&max_edge=224")
                    .body(Body::from("not an image"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(followup.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    async fn assert_client_disconnect_cancels_blocking_work_and_keeps_slot_until_exit(
        connection: &str,
    ) {
        let work = Arc::new(DisconnectTestWork::default());
        let _release = ReleaseOnDrop(work.clone());
        let state = Arc::new(DisconnectTestState {
            slot: Arc::new(Semaphore::new(1)),
            work: work.clone(),
            cancellation: Arc::new(ProtectionState::default()),
        });
        let app = Router::new()
            .route("/work", post(disconnect_test_worker))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, shutdown_signal) = tokio::sync::oneshot::channel();
        let server_app = app.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, server_app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_signal.await;
                })
                .await
        });
        let mut client = TcpStream::connect(address).unwrap();
        write!(
            client,
            "POST /work HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: {connection}\r\n\r\n"
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while work.started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(client);
        let disconnected = tokio::time::timeout(Duration::from_secs(2), async {
            while !state.cancellation.is_cancelled.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        if !disconnected {
            work.release.store(true, Ordering::SeqCst);
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                while !work.finished.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let _ = shutdown.send(());
            let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
            panic!(
                "the request future survived a disconnected client with Connection: {connection}"
            );
        }
        assert!(!work.finished.load(Ordering::SeqCst));
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/work")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        work.release.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !work.finished.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(work.cancelled.load(Ordering::SeqCst));
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.slot.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/work")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(work.started.load(Ordering::SeqCst), 2);
        let _ = shutdown.send(());
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn client_disconnect_cancels_blocking_work_and_keeps_slot_until_exit() {
        assert_client_disconnect_cancels_blocking_work_and_keeps_slot_until_exit("keep-alive")
            .await;
        assert_client_disconnect_cancels_blocking_work_and_keeps_slot_until_exit("close").await;
    }
}
