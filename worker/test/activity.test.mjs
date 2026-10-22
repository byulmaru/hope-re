import assert from "node:assert/strict";
import { Buffer } from "node:buffer";
import { readFile, writeFile } from "node:fs/promises";
import { it } from "node:test";
import { ApplicationFailure, CancelledFailure } from "@temporalio/common";
import { createActivity } from "../dist/activity.js";
import { inputSchema } from "../dist/contract.js";

const id = "u_00000000-0000-4000-8000-000000000001";
const input = {
  id,
  sourceKey: `uploads/${id}/00000000-0000-4000-8000-000000000002`,
  contentType: "image/png",
  maxBytes: 16777216,
  maxDimension: 8192,
  requestId: "00000000-0000-4000-8000-000000000003",
  maxEdge: 1024,
  hope: { algorithm: "noise", iterations: 2, intensity: 0.3 },
};

function setup() {
  const controller = new AbortController();
  const source = { length: 3, contentType: "image/png", etag: "immutable", modified: Date.now() - 1000, bytes: Buffer.from("raw") };
  const objects = new Map([[input.sourceKey, source]]);
  let calls = 0;
  const store = {
    head: async key => objects.get(key),
    read: async key => objects.get(key),
    put: async (key, bytes, fingerprint) => {
      assert.ok(key.startsWith(`${input.sourceKey}.hope/`));
      assert.equal(objects.has(key), false);
      objects.set(key, { bytes, fingerprint, length: bytes.length, etag: "output", modified: Date.now(), contentType: "image/webp" });
      return true;
    },
  };
  const engine = async (request) => {
    if (request.mode === "protect") {
      calls++;
      assert.equal((await readFile(request.inputPath)).toString(), "raw");
      await writeFile(request.outputPath, "valid-webp");
    }
    else { assert.equal((await readFile(request.inputPath)).toString(), "valid-webp"); }
    return { modelUsed: request.mode === "protect", calls: 8, width: 32, height: 24 };
  };
  const prepare = createActivity(store, engine, () => ({ signal: controller.signal, heartbeat() {} }));
  return { prepare, store, objects, source, controller, calls: () => calls };
}

it("strict input rejects mismatched IDs, credentials, invalid budgets and null Hope", () => {
  for (const change of [
    { id: "other" },
    { token: "secret" },
    { maxEdge: 2048 },
    { maxDimension: 8193 },
    { maxBytes: 16777217 },
    { hope: null },
    { hope: { algorithm: "noise", iterations: 501 } },
    { hope: { algorithm: "glaze", iterations: 601 } },
    { hope: { algorithm: "nightshade", iterations: 751 } },
    { hope: { algorithm: "noise", iterations: 1, intensity: 0 } },
  ]) assert.equal(inputSchema.safeParse({ ...input, ...change }).success, false);
  assert.equal(inputSchema.parse({ ...input, hope: { algorithm: "glaze", iterations: 1 } }).hope.intensity, 0.3);
});

it("private conditional output is idempotently reused without rerunning inference", async () => {
  const state = setup();
  assert.equal(await state.prepare(input), undefined);
  await state.prepare(input);
  assert.equal(state.calls(), 1);
  assert.equal(state.objects.size, 2);
  assert.equal(state.objects.get(input.sourceKey), state.source);
});

it("changed options or invalid existing output never overwrite last success", async () => {
  const state = setup();
  await state.prepare(input);
  const saved = [...state.objects.values()][1];
  await assert.rejects(state.prepare({ ...input, hope: { ...input.hope, iterations: 3 } }), error => error instanceof ApplicationFailure && error.nonRetryable);
  assert.equal([...state.objects.values()][1], saved);
  saved.bytes = Buffer.from("invalid");
  await assert.rejects(state.prepare(input));
  assert.equal(state.calls(), 1);
});

it("cancellation during PUT waits for write settlement before acknowledging", async () => {
  const state = setup();
  let release;
  let started;
  const writing = new Promise((resolve) => {
    started = resolve;
  });
  const drained = new Promise((resolve) => {
    release = resolve;
  });
  state.store.put = async () => {
    started();
    await drained;
    return true;
  };
  const activity = state.prepare(input);
  await writing;
  let settled = false;
  const result = activity.finally(() => {
    settled = true;
  });
  const rejection = assert.rejects(result, CancelledFailure);
  state.controller.abort();
  await new Promise(resolve => setTimeout(resolve, 30));
  assert.equal(settled, false);
  await assert.rejects(state.prepare(input), error => error.type === "HopeProcessingError" && !error.nonRetryable);
  release();
  await rejection;
  assert.equal(settled, true);
});

it("source validation is terminal, provider failure retryable and redacted", async () => {
  const state = setup();
  state.source.contentType = "image/jpeg";
  await assert.rejects(state.prepare(input), error => error.type === "UploadValidationError" && error.nonRetryable);
  state.store.head = async () => {
    throw new Error("https://provider/?token=secret raw response");
  };
  await assert.rejects(state.prepare(input), (error) => {
    assert.equal(error.type, "HopeProcessingError");
    assert.equal(error.nonRetryable, false);
    assert.equal(error.cause, undefined);
    assert.doesNotMatch(error.message, /provider|secret|token/);
    return true;
  });
});

it("expired original cannot be processed or reused", async () => {
  const state = setup();
  state.source.modified = Date.now() - 24 * 3600 * 1000;
  await assert.rejects(state.prepare(input), error => error.type === "UploadExpiredError" && error.nonRetryable);
  assert.equal(state.calls(), 0);
});

it("invalid provider credentials fail terminally without provider details", async () => {
  const state = setup();
  state.store.head = async () => {
    throw Object.assign(new Error("credentials at https://provider are invalid"), { $metadata: { httpStatusCode: 403 } });
  };
  await assert.rejects(state.prepare(input), error => error.type === "HopeProcessingError" && error.nonRetryable && !error.cause && !error.message.includes("provider"));
});

it("412 race revalidates the winning immutable output", async () => {
  const state = setup();
  const put = state.store.put;
  state.store.put = async (...args) => {
    await put(...args);
    return false;
  };
  await state.prepare(input);
  assert.equal(state.calls(), 1);
});
