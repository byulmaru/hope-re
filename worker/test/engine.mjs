import assert from "node:assert/strict";
import { mkdtemp, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { env, stdout } from "node:process";
import { createEngine } from "../dist/engine.js";

for (const key of ["HOPE_INFER_BIN", "HOPE_MODEL_DIR", "HOPE_TEST_IMAGE"]) {
  if (!env[key])
    throw new Error(`Missing real-model test configuration: ${key}`);
}
const directory = await mkdtemp(join(tmpdir(), "hope-native-check-"));
const engine = createEngine({ binary: env.HOPE_INFER_BIN, modelDir: env.HOPE_MODEL_DIR, lockPath: join(directory, "lock") });
const request = {
  mode: "protect",
  inputPath: env.HOPE_TEST_IMAGE,
  outputPath: join(directory, "cancelled.webp"),
  contentType: "image/png",
  maxBytes: 16777216,
  maxDimension: 8192,
  maxEdge: 1024,
  hope: { algorithm: "noise", iterations: 50, intensity: 0.3 },
};
const controller = new AbortController();
const started = Date.now();
try {
  const running = engine(request, controller.signal);
  const rejection = assert.rejects(running, error => error.name === "AbortError");
  await new Promise(resolve => setTimeout(resolve, 3000));
  await assert.rejects(engine({ ...request, outputPath: join(directory, "second.webp") }, new AbortController().signal), error => error.type === "HopeProcessingError" && !error.nonRetryable);
  controller.abort();
  await rejection;
  await assert.rejects(stat(request.outputPath), { code: "ENOENT" });
  stdout.write(`${JSON.stringify({ realModel: true, cancelledChildDrained: true, overlappingInferenceBlocked: true, elapsedMs: Date.now() - started })}\n`);
}
finally {
  controller.abort();
  await rm(directory, { recursive: true, force: true });
}
