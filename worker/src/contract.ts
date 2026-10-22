import { ApplicationFailure } from "@temporalio/common";
import { z } from "zod";

export const MAX_BYTES = 16 * 1024 * 1024;
export const NAMESPACE = "media";
export const TASK_QUEUE = "media-hope";
const uuid = "[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}";
const hope = z.strictObject({
  algorithm: z.enum(["noise", "glaze", "nightshade"]),
  iterations: z.number().int().positive(),
  intensity: z.number().gt(0).max(1).default(0.3),
}).refine(value => value.iterations <= ({ noise: 500, glaze: 600, nightshade: 750 })[value.algorithm]);

export const inputSchema = z.strictObject({
  id: z.string().regex(new RegExp(`^u_${uuid}$`)),
  sourceKey: z.string().regex(new RegExp(`^uploads/u_${uuid}/${uuid}$`)),
  contentType: z.enum(["image/jpeg", "image/png", "image/webp"]),
  maxBytes: z.number().int().positive().max(MAX_BYTES),
  maxDimension: z.number().int().positive().max(8192),
  requestId: z.string().regex(new RegExp(`^${uuid}$`)),
  maxEdge: z.literal(1024),
  hope,
}).refine(value => value.sourceKey.split("/")[1] === value.id);

export type HopeInput = z.infer<typeof inputSchema>;

export function fail(type: string, nonRetryable = true): never {
  const messages: Record<string, string> = {
    UploadProcessingInputError: "Hope processing input is invalid",
    UploadValidationError: "Hope source image is invalid",
    UploadSourceError: "upload source metadata is invalid",
    UploadExpiredError: "upload expired before publication",
    HopeProcessingError: "Hope processing failed",
  };
  throw ApplicationFailure.create({
    type: Object.hasOwn(messages, type) ? type : "HopeProcessingError",
    message: messages[type] ?? messages.HopeProcessingError,
    nonRetryable,
  });
}
