import { createServer } from "node:net";
import { env, stderr, stdout } from "node:process";

import { NativeConnection, Runtime, Worker } from "@temporalio/worker";

import { createActivity } from "./activity.js";
import { NAMESPACE, TASK_QUEUE } from "./contract.js";
import { createEngine } from "./engine.js";
import { createStore } from "./storage.js";

async function run() {
  const required = (name: string) => {
    const value = env[name];
    if (!value)
      throw new Error("Worker configuration is incomplete");
    return value;
  };
  const endpoint = required("R2_ENDPOINT");
  const url = new URL(endpoint);
  if (url.protocol !== "https:" || !/^[0-9a-f]{32}\.r2\.cloudflarestorage\.com$/.test(url.hostname) || url.username || url.password || url.search || url.hash || url.pathname !== "/") {
    throw new Error("Worker R2 configuration is invalid");
  }
  const bucket = required("MEDIA_STAGING_BUCKET");
  if (bucket !== "byulmaru-media-staging")
    throw new Error("Worker requires the private staging bucket");
  required("AWS_ACCESS_KEY_ID");
  required("AWS_SECRET_ACCESS_KEY");
  const address = required("TEMPORAL_ADDRESS");
  const engine = createEngine({ binary: required("HOPE_INFER_BIN"), modelDir: required("HOPE_MODEL_DIR"), lockPath: required("HOPE_INFERENCE_LOCK") });
  const singleton = createServer(socket => socket.destroy());
  await new Promise<void>((resolve, reject) => {
    singleton.once("error", reject);
    singleton.listen(18787, "127.0.0.1", resolve);
  });
  const event = (name: string) => stdout.write(`${JSON.stringify({ event: name })}\n`);
  let connection: NativeConnection | undefined;
  let store: ReturnType<typeof createStore> | undefined;
  try {
    Runtime.install({
      logger: {
        log: () => {},
        trace: () => {},
        debug: () => {},
        info: () => {},
        warn: () => { event("temporal_warning"); },
        error: () => { event("temporal_error"); },
      },
      telemetryOptions: { logging: { forward: { level: "WARN" } } },
    });
    store = createStore({ endpoint, bucket });
    connection = await NativeConnection.connect({ address, tls: env.TEMPORAL_TLS === "true" });
    const worker = await Worker.create({
      connection,
      namespace: NAMESPACE,
      taskQueue: TASK_QUEUE,
      activities: { prepareHopeUpload: createActivity(store, engine) },
      maxConcurrentActivityTaskExecutions: 1,
      maxConcurrentActivityTaskPolls: 1,
      maxHeartbeatThrottleInterval: "4 seconds",
      defaultHeartbeatThrottleInterval: "4 seconds",
      shutdownGraceTime: "31 minutes",
      shutdownForceTime: "32 minutes",
    });
    event("hope_worker_started");
    await worker.run();
  }
  finally {
    try {
      await connection?.close();
    }
    finally {
      store?.close();
      singleton.close();
    }
  }
}

run().catch(() => {
  stderr.write("Hope Worker stopped; check configuration, network and local runtime\n");
  process.exitCode = 1;
});
