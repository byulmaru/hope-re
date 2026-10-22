# Hope HTTP adapter

An optional native CPU service for SNS image uploads. No Node addon, npm package, desktop runtime, or Cloudflare Worker deployment is required. The host calls HTTP and stores the returned WebP.

Media's Temporal integration instead uses the local `hope-infer` executable from
the isolated [Activity Worker](../worker/README.md), without calling this HTTP
service. The HTTP contract remains unchanged.

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

All three sessions are loaded once before listening and reused across requests. `models.json` pins official Hope:RE v2.5.1 sizes and SHA-256 hashes. Missing, corrupt, wrong, or Git LFS pointer weights prevent startup. No runtime auto-download or procedural fallback is used by HTTP. For an existing ONNX Runtime installation, the supported `ORT_LIB_LOCATION` build environment variable can be supplied.

## HTTP contract

`GET /health` returns readiness, CPU provider, and available algorithm names.

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

Unknown, duplicate, mismatched, or invalid query parameters are rejected. There is deliberately no implicit iteration preset: the caller must choose the work budget.

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

Upload limit: 32 MiB, 30-second body-read timeout, maximum input dimensions 8192 per side / 40 million pixels, and maximum processed edge 4096. Exactly one upload/processing request holds the worker slot; concurrent requests get `503` without an internal queue. The SNS owns its bounded queue, retries, storage, and publication state. Heavy work runs off the HTTP executor, so health checks remain responsive.

A disconnected client does not cancel an already-running inference job. Keep client/proxy response timeouts long enough for the selected iterations. For this initial adapter, stop the process to abort a running computation. No inference deadline, job persistence, public CORS, watermark endpoint, GPU mode, or cloud SDK is included.

## Node / Worker caller

This uses ordinary `fetch`, not native Node bindings. A Cloudflare Worker may call a reachable private/authenticated API gateway; the native inference binary itself runs on a separate host.

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

The ignored real-model test exercises the HTTP router for all three models, repeated calls, resize-before-filter, WebP input/output, alpha preservation, and health responsiveness while the worker slot is held. It requires local pinned weights. Ordinary tests cover authorization, invalid requests, oversized bodies, and busy rejection without weights. Desktop checks remain separate.

Baseline desktop limitation: the restored `src-tauri/Cargo.toml` at upstream `a3015bb` uses multiline inline tables that the installed Cargo 1.93.1 rejects. This adapter leaves that upstream manifest untouched and builds independently. A full desktop rebuild is not part of the API validation.

Two iterations in these examples are a smoke-test budget, not a production protection preset. The original desktop render-quality default (75) corresponds to 375 / 450 / 562 iterations. Neither a model call nor visible noise establishes protection effectiveness; survival after lossy WebP encoding and effectiveness against training remain unverified.
