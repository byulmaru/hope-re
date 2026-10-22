import type { EngineRequest } from "./engine.js";
import type { Store, StoredObject } from "./storage.js";

import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { stderr } from "node:process";

import { Context } from "@temporalio/activity";
import { ApplicationFailure, CancelledFailure } from "@temporalio/common";

import { fail, inputSchema, MAX_BYTES } from "./contract.js";

type Engine = (request: EngineRequest, signal: AbortSignal) => Promise<{ modelUsed: boolean; calls: number; width: number; height: number }>;
type ActivityRuntime = { signal: AbortSignal; heartbeat: () => void };

export function createActivity(store: Store, engine: Engine, getRuntime = (): ActivityRuntime => {
  const context = Context.current();
  return { signal: context.cancellationSignal, heartbeat: () => context.heartbeat() };
}) {
  let active = false;
  return async function prepareHopeUpload(rawInput: unknown): Promise<void> {
    if (active)
      fail("HopeProcessingError", false);
    active = true;
    let directory: string | undefined;
    let timer: ReturnType<typeof setInterval> | undefined;
    let runtime: ActivityRuntime | undefined;
    try {
      runtime = getRuntime();
      const current = runtime;
      runtime.signal.throwIfAborted();
      const parsed = inputSchema.safeParse(rawInput);
      if (!parsed.success)
        fail("UploadProcessingInputError");
      const input = parsed.data;
      runtime.heartbeat();
      timer = setInterval(current.heartbeat, 2000);
      const source = await store.head(input.sourceKey, runtime.signal);
      if (source.length > input.maxBytes || source.contentType !== input.contentType)
        fail("UploadValidationError");
      if (!Number.isFinite(source.modified) || source.modified <= 0 || source.modified > Date.now())
        fail("UploadSourceError");
      if (Date.now() - source.modified >= 24 * 60 * 60 * 1000)
        fail("UploadExpiredError");
      const fingerprint = createHash("sha256").update(JSON.stringify({ input, etag: source.etag })).digest("hex");
      const destination = `${input.sourceKey}.hope/${input.requestId}.webp`;
      directory = await mkdtemp(join(tmpdir(), "hope-activity-"));
      const inputPath = join(directory, "source");
      const outputPath = join(directory, "output.webp");
      const validate = async (object: StoredObject) => {
        if (object.contentType !== "image/webp" || object.fingerprint !== fingerprint || object.bytes.length > MAX_BYTES)
          fail("HopeProcessingError");
        const path = join(directory!, "existing.webp");
        await writeFile(path, object.bytes, { mode: 0o600 });
        await engine({ mode: "validate", inputPath: path, contentType: "image/webp", maxBytes: MAX_BYTES, maxDimension: 1024, maxEdge: 1024 }, current.signal);
      };
      const existing = await store.read(destination, runtime.signal);
      if (existing) {
        await validate(existing);
        runtime.signal.throwIfAborted();
        return;
      }
      const original = await store.read(input.sourceKey, runtime.signal, source.etag);
      if (!original || original.etag !== source.etag || original.contentType !== input.contentType || original.bytes.length > input.maxBytes)
        fail("UploadSourceError");
      await writeFile(inputPath, original.bytes, { mode: 0o600 });
      await engine({ mode: "protect", inputPath, outputPath, contentType: input.contentType, maxBytes: input.maxBytes, maxDimension: input.maxDimension, maxEdge: 1024, hope: input.hope }, runtime.signal);
      runtime.signal.throwIfAborted();
      const bytes = await readFile(outputPath);
      if (bytes.length === 0 || bytes.length > MAX_BYTES)
        fail("HopeProcessingError");
      await engine({ mode: "validate", inputPath: outputPath, contentType: "image/webp", maxBytes: MAX_BYTES, maxDimension: 1024, maxEdge: 1024 }, runtime.signal);
      runtime.signal.throwIfAborted();
      const stored = await store.put(destination, bytes, fingerprint);
      runtime.signal.throwIfAborted();
      if (!stored) {
        const raced = await store.read(destination, runtime.signal);
        if (!raced)
          fail("HopeProcessingError", false);
        await validate(raced);
      }
      runtime.signal.throwIfAborted();
    }
    catch (error) {
      if (runtime?.signal.aborted)
        throw new CancelledFailure("Hope processing cancelled");
      if (error instanceof ApplicationFailure)
        throw error;
      const status = (error as { $metadata?: { httpStatusCode?: number } } | null)?.$metadata?.httpStatusCode;
      fail("HopeProcessingError", status === 400 || status === 401 || status === 403);
    }
    finally {
      clearInterval(timer);
      try {
        if (directory)
          await rm(directory, { recursive: true, force: true });
      }
      catch {
        stderr.write("Hope temporary file cleanup failed\n");
      }
      finally { active = false; }
    }
  };
}
