import assert from "node:assert/strict";
import { Buffer } from "node:buffer";
import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { env, stdout } from "node:process";
import { fileURLToPath } from "node:url";
import { DeleteObjectCommand, GetObjectCommand, HeadObjectCommand, PutObjectCommand, S3Client } from "@aws-sdk/client-s3";
import { Client, Connection } from "@temporalio/client";
import { NativeConnection, Worker } from "@temporalio/worker";
import { createActivity } from "../dist/activity.js";
import { createEngine } from "../dist/engine.js";
import { createStore } from "../dist/storage.js";

if (env.HOPE_REAL_INTEGRATION !== "1")
  throw new Error("Set HOPE_REAL_INTEGRATION=1 only after configuring real Temporal and private R2 access");
for (const key of ["TEMPORAL_ADDRESS", "R2_ENDPOINT", "MEDIA_STAGING_BUCKET", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "HOPE_INFER_BIN", "HOPE_MODEL_DIR", "HOPE_INFERENCE_LOCK", "HOPE_TEST_IMAGE"]) {
  if (!env[key])
    throw new Error(`Required integration configuration: ${key}`);
}
assert.equal(env.MEDIA_STAGING_BUCKET, "byulmaru-media-staging");
const s3 = new S3Client({ endpoint: env.R2_ENDPOINT, region: "auto", forcePathStyle: true, maxAttempts: 1 });
const id = `u_${randomUUID()}`;
const input = {
  id,
  sourceKey: `uploads/${id}/${randomUUID()}`,
  contentType: "image/png",
  maxBytes: 16777216,
  maxDimension: 8192,
  requestId: randomUUID(),
  maxEdge: 1024,
  hope: { algorithm: "noise", iterations: 2, intensity: 0.3 },
};
const resultKey = `${input.sourceKey}.hope/${input.requestId}.webp`;
const bucket = env.MEDIA_STAGING_BUCKET;
const store = createStore({ endpoint: env.R2_ENDPOINT, bucket });
const engine = createEngine({ binary: resolve(env.HOPE_INFER_BIN), modelDir: resolve(env.HOPE_MODEL_DIR), lockPath: resolve(env.HOPE_INFERENCE_LOCK) });
const connection = await Connection.connect({ address: env.TEMPORAL_ADDRESS, tls: env.TEMPORAL_TLS === "true" });
const nativeConnection = await NativeConnection.connect({ address: env.TEMPORAL_ADDRESS, tls: env.TEMPORAL_TLS === "true" });
const queue = `hope-check-${randomUUID()}`;
const client = new Client({ connection, namespace: "media" });
const workflowWorker = await Worker.create({ connection: nativeConnection, namespace: "media", taskQueue: queue, workflowsPath: fileURLToPath(new URL("./workflow.cjs", import.meta.url)) });
const nativeReports = [];
const activity = createActivity(store, async (...args) => {
  const report = await engine(...args);
  if (report.modelUsed)
    nativeReports.push(report);
  return report;
});
const worker = await Worker.create({ connection: nativeConnection, namespace: "media", taskQueue: "media-hope", activities: { prepareHopeUpload: activity }, maxConcurrentActivityTaskExecutions: 1, maxConcurrentActivityTaskPolls: 1, maxHeartbeatThrottleInterval: "4 seconds", defaultHeartbeatThrottleInterval: "4 seconds" });
try {
  const bytes = await readFile(env.HOPE_TEST_IMAGE);
  await s3.send(new PutObjectCommand({ Bucket: bucket, Key: input.sourceKey, Body: bytes, ContentType: "image/png", IfNoneMatch: "*" }));
  await worker.runUntil(() => workflowWorker.runUntil(async () => {
    for (let attempt = 0; attempt < 2; attempt++) {
      await client.workflow.execute("checkHope", { workflowId: `hope-check-${randomUUID()}`, taskQueue: queue, args: [input] });
    }
  }));
  assert.equal(nativeReports.length, 1, "retry must reuse the stored artifact without inference");
  assert.ok(nativeReports[0].calls > 0);
  assert.ok(nativeReports[0].width <= 1024 && nativeReports[0].height <= 1024);
  const output = await s3.send(new GetObjectCommand({ Bucket: bucket, Key: resultKey }));
  assert.equal(output.ContentType, "image/webp");
  const result = await output.Body.transformToByteArray();
  assert.equal(Buffer.from(result).subarray(8, 12).toString(), "WEBP");
  assert.ok(result.length > 0 && result.length <= 16777216);
  const source = await s3.send(new HeadObjectCommand({ Bucket: bucket, Key: input.sourceKey }));
  assert.equal(source.ContentLength, bytes.length);
  stdout.write(`${JSON.stringify({ realTemporal: true, realR2: true, realModel: true, reused: true, report: nativeReports[0], outputBytes: result.length })}\n`);
}
finally {
  await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: resultKey }));
  await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: input.sourceKey }));
  store.close();
  s3.destroy();
  await connection.close();
  await nativeConnection.close();
}
