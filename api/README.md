# Hope HTTP adapter

An optional native CPU service for SNS image uploads. No Node addon, npm package, desktop runtime, or Cloudflare Worker deployment is required. The host calls HTTP and stores the returned WebP.

Media owns the Temporal Activity, queue, retries, idempotency, R2, and publication
flow. It sends image bytes to this service and stores the returned WebP through
its existing path. This process has no Temporal worker or storage credentials.

## Keeping upstream updates manageable

The original repository structure, frontend, desktop manifests, and release workflows remain intact. `src/upstream.rs` compiles the existing algorithms, preprocessing, SPSA, tiling, and types directly from `../src-tauri/`; there is no copied solver to synchronize.

Only four upstream Rust files change: `protection/{mod,spsa,tiling,types}.rs`. The change replaces Tauri-specific progress delivery inside the solver with a borrowed callback. The desktop adapter still emits the same event name and payload; HTTP ignores progress events. Model math, parameters, random directions, tile blending, and desktop fallback behavior are unchanged.

After merging upstream, inspect those four callback-related hunks and run the API checks below. Source-file reuse reduces divergence, but upstream signature or file-layout changes can still require adapter updates. API Clippy checks focus on adapter code, leaving imported upstream lint policy unchanged.

## Build and start

Requires Rust 1.93.1+, a C/C++ toolchain, Python 3.8+, and network access for initial dependency/runtime/model downloads. No frontend dependency installation is needed for the API.

From the repository root:

```sh
cargo build --release --locked --manifest-path api/Cargo.toml
python3 api/download_models.py --model-dir api/models
export HOPE_MODEL_DIR="$PWD/api/models"
export HOPE_API_TOKEN="$(openssl rand -hex 32)"
api/target/release/hope-api
```

`HOPE_API_TOKEN` must be a random secret of at least 32 visible ASCII bytes. All routes require `Authorization: Bearer ...`, including health checks. Never put the token into a query string or frontend code. The server defaults to `127.0.0.1:8787`; `HOPE_API_ADDR` can select another IP:port. Keep it on a private network and use a TLS reverse proxy for remote access. The token is not a substitute for TLS, rate limits, or upload authorization in the SNS.

`HOPE_API_ALGORITHM` is optional. When unset, all three algorithms are loaded and available as before. Set it to `noise`, `glaze`, or `nightshade` to verify all three pinned files but load only the selected ONNX session and reject requests for the other algorithms. Invalid values prevent startup. The macOS LaunchAgent template selects Nightshade and binds the backend to loopback; see [macOS service files](macos/README.md).

All three pinned files are size and SHA-256 checked before listening. By default all three sessions are loaded and reused across requests; selected mode loads only its configured session after validating the other files. Missing, corrupt, wrong, or Git LFS pointer weights prevent startup. No runtime auto-download or procedural fallback is used by HTTP. For an existing ONNX Runtime installation, the supported `ORT_LIB_LOCATION` build environment variable can be supplied.

## HTTP contract

`GET /health` returns readiness, CPU provider, and available algorithm names. In selected mode it reports only the configured algorithm.

`POST /protect` accepts raw JPEG/PNG/WebP bytes, not multipart, JSON, or Base64. Query parameters:

| Name                | Value                                                                                 |
| ------------------- | ------------------------------------------------------------------------------------- |
| `algorithm`         | Required: `noise`, `glaze`, or `nightshade`                                           |
| `iterations`        | Required: 1..500 / 600 / 750 respectively                                             |
| `intensity`         | Default 0.3; finite, greater than zero, at most 1                                     |
| `max_edge`          | Default 1280; 1..4096; no upscaling                                                   |
| `quality`           | Default 80; final lossy WebP quality 0..100                                           |
| `glaze_style`       | Glaze only: abstract (default), impressionist, cubist, sketch, watercolor             |
| `nightshade_target` | Nightshade only: dog (default), cat, car, landscape, person, building, food, abstract |

Unknown, duplicate, mismatched, or invalid query parameters are rejected. Selected mode rejects other algorithms with `400`. There is deliberately no implicit iteration preset: the caller must choose the work budget.

```sh
curl --fail-with-body \
  -H "Authorization: Bearer $HOPE_API_TOKEN" \
  -H 'Content-Type: image/png' \
  --data-binary @upload.png \
  'http://127.0.0.1:8787/protect?algorithm=glaze&iterations=2&intensity=0.1&max_edge=1280&quality=80' \
  --output protected.webp
```

Success is `200 image/webp`, with `Cache-Control: no-store` and `X-Hope-Model-Calls`, `X-Hope-Width`, `X-Hope-Height`, `X-Hope-Total-Ms` response headers. Failures return non-2xx; application errors use `{"error":"..."}` JSON, never a substitute image. Do not store the response as an image unless the status and content type indicate success.

The pipeline is **decode/orient -> resize -> original model protection -> final WebP encode**. An already compressed WebP can be submitted, but must be decoded and re-encoded. Prefer original uploads or lossless resized intermediates to avoid two lossy encodes. Alpha is preserved, metadata is stripped, and ICC conversion is the caller's responsibility. Animated WebP/APNG are rejected rather than dropping frames.

Upload limit: 32 MiB, 30-second body-read timeout, maximum input dimensions 8192 per side / 40 million pixels, and maximum processed edge 4096. Exactly one upload/processing request holds the worker slot; concurrent requests get `503` without an internal queue. The caller owns stricter upload limits, its bounded queue, retries, storage, and publication state. Heavy work runs off the HTTP executor, so health checks remain responsive.

A disconnected client cooperatively cancels inference. The solver observes cancellation at iteration and SPSA direction boundaries; the current plus/minus model-evaluation pair must finish first. The single worker slot stays held until the blocking task actually exits. Keep client/proxy response timeouts long enough for the selected iterations. There is no inference deadline or job persistence. Public CORS, watermark endpoints, GPU mode, and cloud SDKs are not included.

## Node / Worker caller

This uses ordinary `fetch`, not native Node bindings. The caller connects through an authenticated private HTTPS endpoint; the native inference binary runs on a separate host.

```js
const response = await fetch(`${filterApi}/protect?algorithm=glaze&iterations=2&max_edge=1280`, {
  method: "POST",
  headers: {
    "Authorization": `Bearer ${filterToken}`,
    "Content-Type": "application/octet-stream",
  },
  body: imageBytes,
});
if (!response.ok || response.headers.get("content-type") !== "image/webp") {
  throw new Error(`Filtering failed: ${response.status}`);
}
const webpBytes = await response.arrayBuffer();
```

The caller then saves `webpBytes` using its existing storage path. No storage credentials reach this service.

## Validation

```sh
cargo fmt --check --manifest-path api/Cargo.toml
cargo test --locked --manifest-path api/Cargo.toml
cargo clippy --locked --manifest-path api/Cargo.toml -- -D warnings
HOPE_MODEL_DIR="$PWD/api/models" cargo test --release --locked --manifest-path api/Cargo.toml -- --ignored --test-threads=1
```

The ignored real-model tests retain repeated HTTP coverage for all three algorithms in default mode and add a Nightshade-only request that resizes a 1280px input to 1024px, expects 576 model calls for two iterations, and checks output pixels against an unprotected WebP baseline. Both require all three local pinned weight files. Ordinary tests cover authorization, selected algorithm health/rejection, oversized bodies, busy handling, and TCP disconnect cancellation without weights. Desktop checks remain separate.

Baseline desktop limitation: the restored `src-tauri/Cargo.toml` at upstream `a3015bb` uses multiline inline tables that the installed Cargo 1.93.1 rejects. This adapter leaves that upstream manifest untouched and builds independently. A full desktop rebuild is not part of the API validation.

Two iterations in these examples are a smoke-test budget, not a production protection preset. The original desktop render-quality default (75) corresponds to 375 / 450 / 562 iterations. Neither a model call nor visible noise establishes protection effectiveness; survival after lossy WebP encoding and effectiveness against training remain unverified.
