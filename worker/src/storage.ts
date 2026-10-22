import { Buffer } from "node:buffer";

import { GetObjectCommand, HeadObjectCommand, PutObjectCommand, S3Client } from "@aws-sdk/client-s3";

import { fail, MAX_BYTES } from "./contract.js";

export type ObjectInfo = {
  length: number;
  contentType: string;
  etag: string;
  modified: number;
  fingerprint?: string;
};
export type StoredObject = ObjectInfo & { bytes: Uint8Array };
export type Store = {
  head: (key: string, signal: AbortSignal) => Promise<ObjectInfo>;
  read: (key: string, signal: AbortSignal, etag?: string) => Promise<StoredObject | undefined>;
  put: (key: string, bytes: Uint8Array, fingerprint: string) => Promise<boolean>;
};

function status(error: unknown): number | undefined {
  return (error as { $metadata?: { httpStatusCode?: number } } | null)?.$metadata?.httpStatusCode;
}

function info(response: { ContentLength?: number; ContentType?: string; ETag?: string; LastModified?: Date; Metadata?: Record<string, string> }): ObjectInfo {
  const { ContentLength: length, ContentType: contentType, ETag: etag, LastModified: modified } = response;
  if (!Number.isSafeInteger(length) || !length || length < 0 || length > MAX_BYTES || !contentType || !etag || !modified || !Number.isFinite(modified.getTime())) {
    fail("UploadSourceError");
  }
  return { length, contentType, etag, modified: modified.getTime(), fingerprint: response.Metadata?.fingerprint };
}

export function createStore(config: { endpoint: string; bucket: string }): Store & { close: () => void } {
  const client = new S3Client({
    endpoint: config.endpoint,
    region: "auto",
    forcePathStyle: true,
    maxAttempts: 1,
    requestHandler: { connectionTimeout: 10_000, requestTimeout: 60_000, socketTimeout: 60_000 },
  });
  const input = (key: string) => ({ Bucket: config.bucket, Key: key });
  return {
    async head(key, signal) {
      try {
        return info(await client.send(new HeadObjectCommand(input(key)), { abortSignal: signal }));
      }
      catch (error) {
        if (status(error) === 404)
          fail("UploadSourceError");
        throw error;
      }
    },
    async read(key, signal, etag) {
      let response;
      try {
        response = await client.send(new GetObjectCommand({ ...input(key), ...(etag ? { IfMatch: etag } : {}) }), { abortSignal: signal });
      }
      catch (error) {
        if (status(error) === 404 && !etag)
          return undefined;
        if (status(error) === 404 || status(error) === 412)
          fail("UploadSourceError");
        throw error;
      }
      const body = response.Body;
      try {
        const metadata = info(response);
        if (!body)
          fail("UploadSourceError");
        const chunks: Uint8Array[] = [];
        let length = 0;
        for await (const chunk of body as AsyncIterable<Uint8Array>) {
          signal.throwIfAborted();
          length += chunk.length;
          if (length > MAX_BYTES || length > metadata.length)
            fail("UploadValidationError");
          chunks.push(chunk);
        }
        if (length !== metadata.length)
          fail("UploadValidationError");
        return { ...metadata, bytes: Buffer.concat(chunks) };
      }
      finally {
        if (body && "destroy" in body && typeof body.destroy === "function")
          body.destroy();
      }
    },
    async put(key, bytes, fingerprint) {
      try {
        await client.send(new PutObjectCommand({
          ...input(key),
          Body: bytes,
          ContentType: "image/webp",
          CacheControl: "private, no-store",
          IfNoneMatch: "*",
          Metadata: { fingerprint },
        }));
        return true;
      }
      catch (error) {
        if (status(error) === 412)
          return false;
        throw error;
      }
    },
    close: () => client.destroy(),
  };
}
