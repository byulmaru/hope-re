import type { Buffer } from "node:buffer";

import { spawn } from "node:child_process";

import { z } from "zod";

import { fail } from "./contract.js";

const reportSchema = z.strictObject({
  modelUsed: z.boolean(),
  calls: z.number().int().nonnegative(),
  width: z.number().int().positive().max(1024),
  height: z.number().int().positive().max(1024),
});
const failureSchema = z.object({
  type: z.enum(["UploadProcessingInputError", "UploadValidationError", "HopeProcessingError"]),
  nonRetryable: z.boolean(),
});

export type EngineRequest = {
  mode: "protect" | "validate";
  inputPath: string;
  outputPath?: string;
  contentType: string;
  maxBytes: number;
  maxDimension: number;
  maxEdge: number;
  hope?: { algorithm: string; iterations: number; intensity: number };
};

export function createEngine(config: { binary: string; modelDir: string; lockPath: string }) {
  return async (request: EngineRequest, signal: AbortSignal) => {
    signal.throwIfAborted();
    const child = spawn(config.binary, [], {
      env: {
        PATH: "/usr/bin:/bin",
        HOPE_MODEL_DIR: config.modelDir,
        HOPE_INFERENCE_LOCK: config.lockPath,
      },
      stdio: ["pipe", "pipe", "ignore"],
    });
    let output = "";
    let invalid = false;
    let killTimer: ReturnType<typeof setTimeout> | undefined;
    const cancel = () => {
      child.kill("SIGTERM");
      killTimer ??= setTimeout(() => child.kill("SIGKILL"), 20_000);
    };
    signal.addEventListener("abort", cancel, { once: true });
    if (signal.aborted)
      cancel();
    child.stdout.on("data", (chunk: Buffer) => {
      if (output.length + chunk.length > 4096) {
        invalid = true;
        cancel();
      }
      else {
        output += chunk.toString("utf8");
      }
    });
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(request));
    try {
      const code = await new Promise<number | null>((resolve) => {
        child.on("error", () => {
          invalid = true;
        });
        child.on("close", resolve);
      });
      signal.throwIfAborted();
      if (invalid)
        fail("HopeProcessingError", false);
      let parsed: unknown;
      try {
        parsed = JSON.parse(output);
      }
      catch { fail("HopeProcessingError", false); }
      if (code !== 0) {
        const failure = failureSchema.safeParse(parsed);
        if (failure.success)
          fail(failure.data.type, failure.data.nonRetryable);
        fail("HopeProcessingError", false);
      }
      const report = reportSchema.safeParse(parsed);
      if (!report.success || (request.mode === "protect" && (!report.data.modelUsed || report.data.calls === 0))) {
        fail("HopeProcessingError");
      }
      return report.data;
    }
    finally {
      signal.removeEventListener("abort", cancel);
      clearTimeout(killTimer);
    }
  };
}
