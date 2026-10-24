mod image_io;
mod protection;
#[allow(dead_code, clippy::all)]
mod upstream;

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
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
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use upstream::types::ProtectionState;

const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

struct AppState {
    models: Mutex<Models>,
    slot: Arc<Semaphore>,
    authorization: String,
    selected_algorithm: Option<String>,
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

async fn authorize(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let actual = request
        .headers()
        .get(header::AUTHORIZATION)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    if !bool::from(actual.ct_eq(state.authorization.as_bytes())) {
        return reject(request, StatusCode::UNAUTHORIZED, "Bearer token required").await;
    }
    next.run(request).await
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

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let algorithms = state
        .selected_algorithm
        .as_ref()
        .map_or_else(|| json!(ALGORITHMS), |algorithm| json!([algorithm]));
    Json(json!({"ready": true, "provider": "cpu", "algorithms": algorithms}))
}

async fn protect(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let started = Instant::now();
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
    let protection_state = Arc::new(ProtectionState::default());
    let mut cancellation = RequestCancellation::new(protection_state.clone());
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let image = image_io::decode(&bytes, options.max_edge)
            .map_err(|message| (StatusCode::UNPROCESSABLE_ENTITY, message))?;
        let mut models = state.models.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Worker unavailable".to_string(),
            )
        })?;
        let (image, calls) = models
            .protect_with_state(&image, &options, &protection_state)
            .map_err(|message| {
                eprintln!("Protection failed: {message}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Protection failed; no image returned".to_string(),
                )
            })?;
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
        .route("/protect", post(protect))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let selected_algorithm = selected_algorithm(std::env::var("HOPE_API_ALGORITHM"))?;
    let token = std::env::var("HOPE_API_TOKEN")
        .map_err(|_| "Set HOPE_API_TOKEN to a random secret of at least 32 bytes")?;
    if token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("HOPE_API_TOKEN must contain at least 32 visible ASCII bytes".into());
    }
    let address: SocketAddr = std::env::var("HOPE_API_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
        .map_err(|_| "Invalid HOPE_API_ADDR")?;
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
        authorization: format!("Bearer {token}"),
        selected_algorithm,
    });
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| error.to_string())?;
    eprintln!("Hope API listening on {address}; CPU, one job at a time; use a trusted network and TLS proxy");
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
            authorization: "Bearer test-secret".into(),
            selected_algorithm: None,
        })
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

    #[tokio::test]
    async fn rejects_unauthorized_invalid_oversized_and_busy_requests() {
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
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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
                        .header(header::AUTHORIZATION, "Bearer test-secret")
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
                    .header(header::AUTHORIZATION, "Bearer test-secret")
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
                    .header(header::AUTHORIZATION, "Bearer test-secret")
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
                    .header(header::AUTHORIZATION, "Bearer test-secret")
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
                    .header(header::AUTHORIZATION, "Bearer test-secret")
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
                    .header(header::AUTHORIZATION, "Bearer test-secret")
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
            authorization: "Bearer test-secret".into(),
            selected_algorithm: None,
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
                            .header(header::AUTHORIZATION, "Bearer test-secret")
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
                    .header(header::AUTHORIZATION, "Bearer test-secret")
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
            authorization: "Bearer test-secret".into(),
            selected_algorithm: Some("nightshade".into()),
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
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/protect?algorithm=nightshade&iterations=2&max_edge=1024")
                    .header(header::AUTHORIZATION, "Bearer test-secret")
                    .body(Body::from(input))
                    .unwrap(),
            )
            .await
            .unwrap();
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
        let _permit = state.slot.clone().try_acquire_owned().unwrap();
        let health = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .header(header::AUTHORIZATION, "Bearer test-secret")
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
        for (authorization, status) in [
            ("invalid", StatusCode::UNAUTHORIZED),
            ("Bearer test-secret", StatusCode::SERVICE_UNAVAILABLE),
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
                        .uri("/protect?algorithm=noise&iterations=1")
                        .header(header::AUTHORIZATION, authorization)
                        .body(Body::new(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(consumed.load(Ordering::Relaxed), 512 * 1024);
        }
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
