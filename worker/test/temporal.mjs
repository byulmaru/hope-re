import assert from "node:assert/strict";
import { Buffer } from "node:buffer";
import { randomUUID } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { env, stdout } from "node:process";
import { fileURLToPath } from "node:url";
import { TestWorkflowEnvironment } from "@temporalio/testing";
import { Runtime, Worker } from "@temporalio/worker";
import { createActivity } from "../dist/activity.js";
import { createEngine } from "../dist/engine.js";

Runtime.install({ logger: { log() {}, trace() {}, debug() {}, info() {}, warn() {}, error() {} } });
const temporalCliDir = join(tmpdir(), "hope-temporal-cli");
await mkdir(temporalCliDir, { recursive: true });
const environment = await TestWorkflowEnvironment.createLocal({ server: { namespace: "media", executable: { type: "cached-download", downloadDir: temporalCliDir } } });
const id = `u_${randomUUID()}`;
const sourceKey = `uploads/${id}/${randomUUID()}`;
const input = { id, sourceKey, requestId: randomUUID(), contentType: "image/png", maxBytes: 16777216, maxDimension: 8192, maxEdge: 1024, hope: { algorithm: "noise", iterations: 2, intensity: 0.3 } };
const sourceBytes = env.HOPE_TEST_IMAGE ? await readFile(env.HOPE_TEST_IMAGE) : Buffer.from("fake-source");
const objects = new Map([[sourceKey, { length: sourceBytes.length, contentType: "image/png", etag: "immutable-source", modified: Date.now() - 1000, bytes: sourceBytes }]]);
let calls = 0;
let release;
let writing;
let pendingWrite;
const store = {
  head: async key => objects.get(key),
  read: async key => objects.get(key),
  put: async (key, bytes, fingerprint) => {
    if (writing) {
      writing();
      await pendingWrite;
    }
    objects.set(key, { length: bytes.length, contentType: "image/webp", etag: "immutable-result", modified: Date.now(), bytes, fingerprint });
    return true;
  },
};
const native = env.HOPE_INFER_BIN ? createEngine({ binary: env.HOPE_INFER_BIN, modelDir: env.HOPE_MODEL_DIR, lockPath: env.HOPE_INFERENCE_LOCK }) : undefined;
async function engine(request, signal) {
  if (request.mode === "protect")
    calls++;
  if (native)
    return native(request, signal);
  if (request.mode === "protect")
    await writeFile(request.outputPath, "fake-output");
  return { modelUsed: true, calls: 8, width: 32, height: 24 };
}
const worker = await Worker.create({
  connection: environment.nativeConnection,
  namespace: "media",
  taskQueue: "media-hope",
  activities: { prepareHopeUpload: createActivity(store, engine) },
  maxConcurrentActivityTaskExecutions: 1,
  maxConcurrentActivityTaskPolls: 1,
  maxHeartbeatThrottleInterval: "4 seconds",
  defaultHeartbeatThrottleInterval: "4 seconds",
});
const workflowWorker = await Worker.create({ connection: environment.nativeConnection, namespace: "media", taskQueue: "hope-local-check", workflowsPath: fileURLToPath(new URL("./workflow.cjs", import.meta.url)) });
try {
  await worker.runUntil(() => workflowWorker.runUntil(async () => {
    for (let attempt = 0; attempt < 2; attempt++) {
      await environment.client.workflow.execute("checkHope", { workflowId: `check-${randomUUID()}`, taskQueue: "hope-local-check", args: [input] });
    }
    assert.equal(calls, 1);
    let started;
    const writeStarted = new Promise((resolve) => {
      started = resolve;
    });
    writing = started;
    pendingWrite = new Promise((resolve) => {
      release = resolve;
    });
    const handle = await environment.client.workflow.start("checkHope", { workflowId: `cancel-${randomUUID()}`, taskQueue: "hope-local-check", args: [{ ...input, requestId: randomUUID() }] });
    await writeStarted;
    await handle.cancel();
    let finished = false;
    const result = handle.result().finally(() => {
      finished = true;
    });
    const rejected = assert.rejects(result);
    await new Promise(resolve => setTimeout(resolve, 6500));
    assert.equal(finished, false, "WAIT_CANCELLATION_COMPLETED must wait for in-flight write");
    release();
    await rejected;
    assert.equal(objects.get(sourceKey).bytes, sourceBytes);
    stdout.write(`${JSON.stringify({ realTemporal: true, realModel: Boolean(native), realR2: false, idempotent: true, cancellationWaitedForWrite: true, inferenceRuns: calls })}\n`);
  }));
}
finally {
  await environment.teardown();
}
