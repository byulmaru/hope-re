import assert from "node:assert/strict";
import { Buffer } from "node:buffer";
import { createServer } from "node:http";
import { env } from "node:process";
import { it } from "node:test";
import { createStore } from "../dist/storage.js";

it("S3 transport enforces immutable reads, conditional private writes and awaits PUT completion", async () => {
  env.AWS_ACCESS_KEY_ID = "test-access-key";
  env.AWS_SECRET_ACCESS_KEY = "test-secret-key";
  delete env.AWS_SESSION_TOKEN;
  let completeWrite;
  let writeStarted;
  const writing = new Promise((resolve) => {
    writeStarted = resolve;
  });
  let exists = false;
  const server = createServer(async (request, response) => {
    assert.ok(request.url.startsWith("/private-staging/"));
    if (request.method === "PUT") {
      assert.equal(request.headers["if-none-match"], "*");
      assert.equal(request.headers["content-type"], "image/webp");
      assert.equal(request.headers["cache-control"], "private, no-store");
      assert.equal(request.headers["x-amz-meta-fingerprint"], "test-fingerprint");
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      assert.equal(Buffer.concat(chunks).toString(), "webp-bytes");
      if (exists) {
        response.writeHead(412);
        response.end();
      }
      else {
        exists = true;
        completeWrite = () => {
          response.writeHead(200, { etag: "\"result-etag\"" });
          response.end();
        };
        writeStarted();
      }
      return;
    }
    if (request.url.includes("missing")) {
      response.writeHead(404);
      response.end();
      return;
    }
    if (request.method === "GET")
      assert.equal(request.headers["if-match"], "\"source-etag\"");
    response.writeHead(200, {
      "content-type": "image/png",
      "content-length": "3",
      "etag": "\"source-etag\"",
      "last-modified": new Date(Date.now() - 1000).toUTCString(),
    });
    response.end(request.method === "HEAD" ? undefined : "raw");
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const store = createStore({ endpoint: `http://127.0.0.1:${server.address().port}`, bucket: "private-staging" });
  const signal = new AbortController().signal;
  try {
    const head = await store.head("source", signal);
    assert.equal(head.length, 3);
    assert.equal((await store.read("source", signal, head.etag)).bytes.toString(), "raw");
    assert.equal(await store.read("missing", signal), undefined);
    let settled = false;
    const put = store.put("output", Buffer.from("webp-bytes"), "test-fingerprint").finally(() => {
      settled = true;
    });
    await writing;
    await new Promise(resolve => setTimeout(resolve, 30));
    assert.equal(settled, false);
    completeWrite();
    assert.equal(await put, true);
    assert.equal(await store.put("output", Buffer.from("webp-bytes"), "test-fingerprint"), false);
  }
  finally {
    store.close();
    await new Promise(resolve => server.close(resolve));
  }
});
