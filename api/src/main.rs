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
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

struct AppState {
    models: Mutex<Models>,
    slot: Arc<Semaphore>,
    authorization: String,
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
        let (image, calls) = models.protect(&image, &options).map_err(|message| {
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
        .route(
            "/health",
            get(|| async {
                Json(json!({"ready": true, "provider": "cpu", "algorithms": ALGORITHMS}))
            }),
        )
        .route("/protect", post(protect))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

#[tokio::main]
async fn main() -> Result<(), String> {
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
    let state = Arc::new(AppState {
        models: Mutex::new(Models::load(Path::new(&directory))?),
        slot: Arc::new(Semaphore::new(1)),
        authorization: format!("Bearer {token}"),
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
    use tower::ServiceExt;

    fn state() -> Arc<AppState> {
        Arc::new(AppState {
            models: Mutex::new(Models::empty_for_test()),
            slot: Arc::new(Semaphore::new(1)),
            authorization: "Bearer test-secret".into(),
        })
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
    #[ignore = "requires official weights in HOPE_MODEL_DIR"]
    async fn real_models_handle_repeated_http_requests() {
        let directory = std::env::var("HOPE_MODEL_DIR").expect("set HOPE_MODEL_DIR");
        let state = Arc::new(AppState {
            models: Mutex::new(Models::load(Path::new(&directory)).unwrap()),
            slot: Arc::new(Semaphore::new(1)),
            authorization: "Bearer test-secret".into(),
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
}
