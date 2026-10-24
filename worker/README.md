# Media Hope Activity Worker

Native CPU inference for PROD-1151. Media schedules `prepareHopeUpload` on namespace
`media`, task queue `media-hope`. This package uses the same Temporal TypeScript
SDK version as Media PR #20. It invokes the local `hope-infer` Rust executable;
there is no HTTP service or Media-side Hope deployment.

## Ownership and contract

The strict input is `{ id, sourceKey, contentType, maxBytes, maxDimension,
requestId, maxEdge: 1024, hope: { algorithm, iterations, intensity } }`.
IDs are lowercase UUIDv4 values, with `u_` for the upload ID. The source key must
be `uploads/<id>/<UUIDv4>`. Algorithms are `noise`, `glaze`, `nightshade`, with
iteration limits 500, 600, 750. Intensity defaults to 0.3 and must be in `(0, 1]`.
Only JPEG, PNG and still WebP are accepted. Original limits are 16 MiB, 8192px per
side and 16 MiPixels. EXIF orientation is applied before reducing the longest
edge to at most 1024px without upscaling. Glaze uses abstract style; Nightshade
uses the dog target, matching the existing adapter defaults.

The native executable verifies the official model sizes and SHA-256 hashes from
`api/models.json` on every inference invocation, then runs the existing upstream
solver without fallback. It encodes one final WebP at quality 80, strips metadata
and preserves alpha. Existing HTTP decoding limits and defaults are unchanged.

Source HEAD/GET must agree on MIME, size and immutable ETag. Sources older than
24 hours or with future/invalid timestamps are rejected. Result objects are
stored only in `byulmaru-media-staging`, at
`${sourceKey}.hope/${requestId}.webp`, with `If-None-Match: *`, private cache
metadata and an options/source-ETag fingerprint. A retry fully decodes an
existing result and checks its fingerprint, MIME, 16 MiB and 1024px limits before
reusing it. A conditional-write race revalidates the winner. Invalid existing
objects are never overwritten. The Worker returns void only after storage has
completed. It never publishes, deletes or changes the original. Media owns
`hope: null`, public promotion, preservation of the previous selected result,
publication blocking after failure and all artifact cleanup.

Input/source/image validation failures are non-retryable typed
`ApplicationFailure`s. Configuration/model-integrity and deterministic numeric
inference failures are terminal `HopeProcessingError`s; temporary storage,
process/resource and inference-resource failures are retryable. Error messages
are fixed public strings without causes. Provider responses, endpoints,
credentials and image bytes are not logged. Native children receive no R2 or
Temporal credentials and their stderr is discarded.

Activity heartbeat calls run every two seconds, with SDK heartbeat throttling
capped at four seconds. Media owns the 30-second heartbeat timeout, per-attempt
30-minute bound, remaining-source-lifetime deadline plus five-minute cleanup
grace, retries and `WAIT_CANCELLATION_COMPLETED`. Reads can abort; inference
receives SIGTERM cooperatively through the existing solver cancellation state.
After 20 seconds an unresponsive child is killed, and its close event is still
awaited. An already-started R2 PUT is always awaited before cancellation is
acknowledged. No fire-and-forget object writes are used.

## Container image

Build from the repository root with:

```sh
docker build -f worker/Dockerfile -t hope-re-worker .
```

The image contains Node 24, the native `hope-infer` binary and all three official
models. The build downloads and verifies each model against `api/models.json`; the
container does not fetch models at runtime. The image targets `linux/arm64` only.
Pull requests build the image and smoke-test all three models on a native
`ubuntu-24.04-arm` runner; pushes to `main` publish it directly as
`ghcr.io/byulmaru/hope-re-worker:<commit-sha>`.

The process runs as UID/GID `10001:10001`. Node starts as PID 1 so SIGTERM reaches
the Temporal SDK and its child process. The image defaults are
`HOPE_INFER_BIN=/app/bin/hope-infer`, `HOPE_MODEL_DIR=/app/models` and
`HOPE_INFERENCE_LOCK=/tmp/hope-inference.lock`.

Provide `TEMPORAL_ADDRESS`, `TEMPORAL_TLS`, `R2_ENDPOINT`,
`MEDIA_STAGING_BUCKET`, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` through
the deployment environment. `AWS_SESSION_TOKEN` is optional. Kubernetes/Vault
configuration must inject secret values at runtime; no credentials are included
in the image. See the [Kubernetes Helm chart](helm/).

## Build and checks

Use native Node 24 and Rust 1.93.1 or later. This is an independent package; do not
replace the desktop application's dependencies or Cargo manifest.

```sh
cargo build --manifest-path api/Cargo.toml --release --locked --bin hope-infer
python3 api/download_models.py --model-dir api/models
cd worker
npm ci
npm test
npm run test:temporal
```

Pull requests touching `worker/` run these Node checks plus Helm 4.2.2 lint and
render assertions. The image workflow builds and smoke-tests `linux/arm64` when
an image input or the smoke test changes.

`npm test` uses fake storage/inference for focused failure and cancellation
checks. `test:temporal` downloads a local Temporal development server under
`hope-temporal-cli` in the OS temporary directory; it tests real scheduling,
retry reuse and pending-write cancellation with memory storage. To use real
inference in that test, export absolute `HOPE_INFER_BIN`, `HOPE_MODEL_DIR`,
`HOPE_INFERENCE_LOCK` and `HOPE_TEST_IMAGE` (a PNG). It still does not test real
R2.

With those variables, `npm run test:engine` also checks a real running child's
cooperative SIGTERM drain and rejection of a second concurrent inference process.

```sh
HOPE_TEST_IMAGE=/absolute/input.png \
HOPE_MODEL_DIR=/absolute/models \
HOPE_INFERENCE_LOCK=/private/tmp/hope-check.lock \
cargo test --manifest-path api/Cargo.toml --release --bin hope-infer \
  real_models_change_pixels_and_honor_cancellation -- --ignored --nocapture
```

That check runs all three actual models, reports dimensions/calls/pixel changes
and checks pre-cancelled solver behavior. Two iterations are diagnostic; they do
not establish protection effectiveness or a production iteration preset.

## Deployment

The macOS LaunchAgent and Kubernetes Deployment are mutually exclusive choices
for the production poller. Keep exactly one active Worker polling namespace
`media`, task queue `media-hope`. The local singleton socket and inference lock
do not coordinate across hosts or replicas.

The Temporal web dashboard is not the gRPC frontend. The current dashboard peer
does not accept TCP 7233. A cluster operator must expose the Temporal frontend's
TCP 7233 through the existing Tailscale operator or an approved private route,
with a narrowly scoped network policy/ACL. Do not make it publicly accessible,
use the dashboard HTTPS endpoint as a gRPC address, or silently change another
repository's infrastructure. Verify the destination cluster and namespace
`media` before starting the Worker. `TEMPORAL_TLS` must match that frontend; a
plain TCP frontend is transported inside the encrypted Tailscale path.

### macOS-only LaunchAgent

Create a macOS Keychain generic-password item with service
`co.byulmaru.hope-worker`. Its password is a JSON object containing:

| Field                   | Value                                        |
| ----------------------- | -------------------------------------------- |
| `TEMPORAL_ADDRESS`      | Approved Tailnet frontend host and gRPC port |
| `TEMPORAL_TLS`          | `true` for TLS, otherwise `false`            |
| `R2_ENDPOINT`           | Account R2 HTTPS endpoint, not a public CDN  |
| `MEDIA_STAGING_BUCKET`  | `byulmaru-media-staging`                     |
| `AWS_ACCESS_KEY_ID`     | Private staging bucket credential            |
| `AWS_SECRET_ACCESS_KEY` | Private staging bucket secret                |
| `AWS_SESSION_TOKEN`     | Optional temporary-session token             |

Use Keychain Access or an existing trusted secret provisioning process. Do not
paste secrets into chat, commit them, put them in a plist, or pass secret JSON on
the command line. Scope the credential to private staging object access; the
Worker does not require public-bucket access. Never copy a production token into
the test source payload. Direct foreground execution can read these same fields
from the process environment without Keychain for development.

```sh
python3 worker/macos/service.py install --node /absolute/node24/bin/node
launchctl print gui/$(id -u)/co.byulmaru.hope-worker
python3 worker/macos/service.py stop
```

The installer validates build/model/Keychain prerequisites, drains/unloads the
old launchd job before loading its replacement, and refuses a timed-out stop.
KeepAlive restarts failures, with a 30-second throttle. ExitTimeOut exceeds the
Worker's graceful-shutdown window. A fixed localhost singleton socket prevents
two Node Worker processes on this host. A native exclusive OS file lock at
`~/Library/Application Support/co.byulmaru.hope-worker/inference.lock` prevents
overlapping inference even if the parent dies before its child exits. Logs under
`~/Library/Logs/co.byulmaru.hope-worker` contain fixed lifecycle/warning/error
event names, not provider diagnostics.

The Mac LaunchAgent runs per login user: log out or sleep stops availability.
Ensure the Mac mini remains awake and the intended user session is active. A
daemon or unattended boot credential policy is a separate deployment decision.
Service registration or a healthy process alone is not proof of integration
readiness.

## Real integration gate

Keep Media `hope.enabled=false` until the real end-to-end gate passes. The
opt-in `npm run test:integration` requires `HOPE_REAL_INTEGRATION=1`, the real
Temporal/R2 environment above and `HOPE_INFER_BIN`, `HOPE_MODEL_DIR`,
`HOPE_INFERENCE_LOCK`, and `HOPE_TEST_IMAGE`. Use the private
`byulmaru-media-staging` bucket and a staging-only credential with access to its
temporary test objects; never pass secret values as command-line arguments.

The test runner starts its own Activity Worker on `media-hope`, so it must be
the only poller for that queue. Before starting it, stop the macOS LaunchAgent
if it is running and coordinate a staging maintenance window with the GitOps
operator. Pause reconciliation for the `hope-worker` Deployment so GitOps does
not restore its fixed one-replica target during the test. Do not pause or change
other resources in `media`.

Scale the Kubernetes Worker to zero and wait for every Pod to disappear through
its full termination grace period. The chart allows 1950 seconds for shutdown;
use at least 33 minutes and do not force-delete Pods.

```sh
kubectl scale deployment/hope-worker -n media --replicas=0
kubectl wait --for=delete pod -n media -l app.kubernetes.io/name=hope-worker --timeout=33m
kubectl get pods -n media -l app.kubernetes.io/name=hope-worker
```

Run the test only after `kubectl get pods` reports no matching Pods:

```sh
cd worker
HOPE_REAL_INTEGRATION=1 npm run test:integration
```

The test creates fresh private source/result keys, runs actual inference, then
repeats the same request to verify idempotence. Its `finally` block deletes only
those two test objects. It never publishes an image. If the wait times out, any
Pod remains, or the test process is still running, do not start another poller.

After the test process exits, restore one Worker even if the test failed, wait
for the Deployment rollout, and verify the started event and queue polling
through the existing Temporal tooling. Then resume GitOps reconciliation and
confirm the deployment remains at one active replica.

```sh
kubectl scale deployment/hope-worker -n media --replicas=1
kubectl rollout status deployment/hope-worker -n media --timeout=33m
kubectl logs deployment/hope-worker -n media --since=5m
```

Finish acceptance with Media's actual Workflow/Nexus path, failure after a prior
successful result, no public publication on failure, pending-write cancellation
and an observed graceful drain and restart with no overlapping pollers. A
local dev Temporal run or fake R2 tests cannot check these operational conditions.
Do not claim rollout, production R2 or Media acceptance from the local checks alone.
