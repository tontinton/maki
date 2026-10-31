/**
 * Cursor model discovery via GetUsableModels.
 * Uses the H2 bridge for transport. Falls back to a hardcoded list
 * when discovery fails.
 */
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { z } from "zod";
import { callCursorUnaryRpc } from "./proxy.ts";
import {
  GetUsableModelsRequestSchema,
  GetUsableModelsResponseSchema,
} from "./proto/agent_pb.ts";

const GET_USABLE_MODELS_PATH = "/agent.v1.AgentService/GetUsableModels";

const DEFAULT_CONTEXT_WINDOW = 200_000;
const DEFAULT_MAX_TOKENS = 64_000;

const CursorModelDetailsSchema = z.object({
  modelId: z.string(),
  displayName: z.string().optional().catch(undefined),
  displayNameShort: z.string().optional().catch(undefined),
  displayModelId: z.string().optional().catch(undefined),
  aliases: z
    .array(z.unknown())
    .optional()
    .catch([])
    .transform((aliases) =>
      (aliases ?? []).filter(
        (alias: unknown): alias is string => typeof alias === "string",
      ),
    ),
  thinkingDetails: z.unknown().optional(),
});

type CursorModelDetails = z.infer<typeof CursorModelDetailsSchema>;

export interface CursorModel {
  id: string;
  name: string;
  reasoning: boolean;
  contextWindow: number;
  maxTokens: number;
}

const FALLBACK_MODELS: CursorModel[] = [
  // Composer models
  { id: "composer-2.5", name: "Composer 2.5", reasoning: true, contextWindow: 200_000, maxTokens: 64_000 },
  { id: "composer-1", name: "Composer 1", reasoning: true, contextWindow: 200_000, maxTokens: 64_000 },
  { id: "composer-1.5", name: "Composer 1.5", reasoning: true, contextWindow: 200_000, maxTokens: 64_000 },
  // Claude models
  { id: "claude-4.6-opus-high", name: "Claude 4.6 Opus", reasoning: true, contextWindow: 200_000, maxTokens: 128_000 },
  { id: "claude-4.6-sonnet-medium", name: "Claude 4.6 Sonnet", reasoning: true, contextWindow: 200_000, maxTokens: 64_000 },
  { id: "claude-4.5-sonnet", name: "Claude 4.5 Sonnet", reasoning: true, contextWindow: 200_000, maxTokens: 64_000 },
  // GPT models
  { id: "gpt-5.4-medium", name: "GPT-5.4", reasoning: true, contextWindow: 272_000, maxTokens: 128_000 },
  { id: "gpt-5.2", name: "GPT-5.2", reasoning: true, contextWindow: 400_000, maxTokens: 128_000 },
  { id: "gpt-5.2-codex", name: "GPT-5.2 Codex", reasoning: true, contextWindow: 400_000, maxTokens: 128_000 },
  { id: "gpt-5.3-codex", name: "GPT-5.3 Codex", reasoning: true, contextWindow: 400_000, maxTokens: 128_000 },
  { id: "gpt-5.3-codex-spark-preview", name: "GPT-5.3 Codex Spark", reasoning: true, contextWindow: 128_000, maxTokens: 128_000 },
  // Other models
  { id: "gemini-3.1-pro", name: "Gemini 3.1 Pro", reasoning: true, contextWindow: 1_000_000, maxTokens: 64_000 },
  { id: "grok-code-fast-1", name: "Grok Code Fast 1", reasoning: false, contextWindow: 128_000, maxTokens: 64_000 },
];

/**
 * Pseudo-model for Cursor's server-side Auto routing. Always exposed
 * alongside discovered models; the proxy maps it to Run modelId "default".
 */
const AUTO_MODEL: CursorModel = {
  id: "auto",
  name: "Auto",
  reasoning: false,
  contextWindow: DEFAULT_CONTEXT_WINDOW,
  maxTokens: DEFAULT_MAX_TOKENS,
};

async function fetchCursorUsableModels(
  apiKey: string,
): Promise<CursorModel[] | null> {
  try {
    const requestPayload = create(GetUsableModelsRequestSchema, {});
    const requestBody = toBinary(GetUsableModelsRequestSchema, requestPayload);

    const response = await callCursorUnaryRpc({
      accessToken: apiKey,
      rpcPath: GET_USABLE_MODELS_PATH,
      requestBody,
    });

    if (response.timedOut || response.exitCode !== 0 || response.body.length === 0) {
      return null;
    }

    const decoded = decodeGetUsableModelsResponse(response.body);
    if (!decoded) return null;

    const models = normalizeCursorModels(decoded.models);
    return models.length > 0 ? models : null;
  } catch {
    return null;
  }
}

let cachedModels: CursorModel[] | null = null;

export async function getCursorModels(
  apiKey: string,
): Promise<CursorModel[]> {
  if (cachedModels) return cachedModels;
  const discovered = apiKey ? await fetchCursorUsableModels(apiKey) : null;
  const models = discovered && discovered.length > 0 ? discovered : FALLBACK_MODELS;
  cachedModels = models.some((m) => m.id === AUTO_MODEL.id) ? models : [AUTO_MODEL, ...models];
  return cachedModels;
}

/** @internal Test-only. */
export function clearModelCache(): void {
  cachedModels = null;
}

function decodeGetUsableModelsResponse(payload: Uint8Array): {
  models: readonly unknown[];
} | null {
  try {
    return fromBinary(GetUsableModelsResponseSchema, payload);
  } catch {
    const framedBody = decodeConnectUnaryBody(payload);
    if (!framedBody) return null;
    try {
      return fromBinary(GetUsableModelsResponseSchema, framedBody);
    } catch {
      return null;
    }
  }
}

function decodeConnectUnaryBody(payload: Uint8Array): Uint8Array | null {
  if (payload.length < 5) return null;

  let offset = 0;
  while (offset + 5 <= payload.length) {
    const flags = payload[offset]!;
    const view = new DataView(
      payload.buffer,
      payload.byteOffset + offset,
      payload.byteLength - offset,
    );
    const messageLength = view.getUint32(1, false);
    const frameEnd = offset + 5 + messageLength;
    if (frameEnd > payload.length) return null;

    // Compression flag
    if ((flags & 0b0000_0001) !== 0) return null;

    // End-of-stream flag — skip trailer frames
    if ((flags & 0b0000_0010) === 0) {
      return payload.subarray(offset + 5, frameEnd);
    }

    offset = frameEnd;
  }

  return null;
}

function normalizeCursorModels(
  models: readonly unknown[],
): CursorModel[] {
  if (models.length === 0) return [];

  const byId = new Map<string, CursorModel>();
  for (const model of models) {
    const normalized = normalizeSingleModel(model);
    if (normalized) byId.set(normalized.id, normalized);
  }
  return [...byId.values()].sort((a, b) => a.id.localeCompare(b.id));
}

function normalizeSingleModel(model: unknown): CursorModel | null {
  const parsed = CursorModelDetailsSchema.safeParse(model);
  if (!parsed.success) return null;

  const details = parsed.data;
  const id = details.modelId.trim();
  if (!id) return null;

  return {
    id,
    name: pickDisplayName(details, id),
    reasoning: Boolean(details.thinkingDetails),
    contextWindow: DEFAULT_CONTEXT_WINDOW,
    maxTokens: DEFAULT_MAX_TOKENS,
  };
}

function pickDisplayName(model: CursorModelDetails, fallbackId: string): string {
  const candidates = [
    model.displayName,
    model.displayNameShort,
    model.displayModelId,
    ...model.aliases,
    fallbackId,
  ];
  for (const candidate of candidates) {
    if (typeof candidate !== "string") continue;
    const trimmed = candidate.trim();
    if (trimmed) return trimmed;
  }
  return fallbackId;
}
