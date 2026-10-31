/**
 * Local OpenAI-compatible proxy that translates requests to Cursor's gRPC protocol.
 *
 * Accepts POST /v1/chat/completions in OpenAI format, translates to Cursor's
 * protobuf/HTTP2 Connect protocol, and streams back OpenAI-format SSE.
 *
 * Tool calling uses Cursor's native MCP tool protocol:
 * - OpenAI tool defs → McpToolDefinition in RequestContext
 * - Cursor toolCallStarted/Delta/Completed → OpenAI tool_calls SSE chunks
 * - mcpArgs exec → pause stream, return tool_calls to caller
 * - Follow-up request with tool results → resume bridge with mcpResult
 *
 * HTTP/2 transport is delegated to a Node child process (h2-bridge.mjs)
 * because Bun's node:http2 module is broken.
 */
import { create, fromBinary, fromJson, type JsonValue, toBinary, toJson } from "@bufbuild/protobuf";
import { ValueSchema } from "@bufbuild/protobuf/wkt";
import {
  AgentClientMessageSchema,
  AgentRunRequestSchema,
  AgentServerMessageSchema,
  ClientHeartbeatSchema,
  ConversationActionSchema,
  ConversationStateStructureSchema,
  ConversationStepSchema,
  AgentConversationTurnStructureSchema,
  ConversationTurnStructureSchema,
  AssistantMessageSchema,
  BackgroundShellSpawnResultSchema,
  DeleteResultSchema,
  DeleteRejectedSchema,
  DiagnosticsResultSchema,
  ExecClientMessageSchema,
  FetchErrorSchema,
  FetchResultSchema,
  GetBlobResultSchema,
  GrepErrorSchema,
  GrepResultSchema,
  KvClientMessageSchema,
  LsRejectedSchema,
  LsResultSchema,
  McpErrorSchema,
  McpResultSchema,
  McpSuccessSchema,
  McpTextContentSchema,
  McpToolDefinitionSchema,
  McpToolResultContentItemSchema,
  ModelDetailsSchema,
  ReadRejectedSchema,
  ReadResultSchema,
  RequestContextResultSchema,
  RequestedModelSchema,
  RequestContextSchema,
  RequestContextSuccessSchema,
  ResumeActionSchema,
  SetBlobResultSchema,
  ShellRejectedSchema,
  ShellResultSchema,
  UserMessageActionSchema,
  UserMessageSchema,
  WriteRejectedSchema,
  WriteResultSchema,
  WriteShellStdinErrorSchema,
  WriteShellStdinResultSchema,
  type AgentServerMessage,
  type ConversationStateStructure,
  type ExecServerMessage,
  type KvServerMessage,
  type McpToolDefinition,
} from "./proto/agent_pb.ts";
import {
  redirectNativeExec,
  sendNativeExecResult,
  type NativeExecBinding,
} from "./native-tools.ts";
import { spawn, type ChildProcess } from "node:child_process";
import http from "node:http";
import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, stat, unlink, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { resolve as pathResolve } from "node:path";
import { z } from "zod";
import { deriveBridgeKey, deriveConversationKey } from "./session-key.ts";

const CURSOR_API_URL = process.env.CURSOR_API_URL ?? "https://api2.cursor.sh";
const CONNECT_END_STREAM_FLAG = 0b00000010;
const BRIDGE_PATH = pathResolve(import.meta.dirname, "h2-bridge.mjs");
const SSE_HEADERS = {
  "Content-Type": "text/event-stream",
  "Cache-Control": "no-cache",
  Connection: "keep-alive",
} as const;

interface OpenAIToolCall {
  id: string;
  type: "function";
  function: { name: string; arguments: string };
}

/** A single element in an OpenAI multi-part content array. */
interface ContentPart {
  type: string;
  text?: string;
}

interface OpenAIMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string | null | ContentPart[];
  tool_call_id?: string;
  tool_calls?: OpenAIToolCall[];
}

interface OpenAIToolDef {
  type: "function";
  function: {
    name: string;
    description?: string;
    parameters?: Record<string, unknown>;
  };
}

interface ChatCompletionRequest {
  model: string;
  messages: OpenAIMessage[];
  stream?: boolean;
  temperature?: number;
  max_tokens?: number;
  tools?: OpenAIToolDef[];
  tool_choice?: unknown;
}


interface CursorRequestPayload {
  requestBytes: Uint8Array;
  blobStore: Map<string, Uint8Array>;
  mcpTools: McpToolDefinition[];
  /** System prompt forwarded via RequestContext.cloudRule (issue #21). */
  cloudRule?: string;
}

/** A pending tool execution waiting for results from the caller. */
interface PendingExec {
  execId: string;
  execMsgId: number;
  toolCallId: string;
  toolName: string;
  /** Decoded arguments JSON string for SSE tool_calls emission. */
  decodedArgs: string;
  /** Set when this exec is a redirected native tool call (issue #21/#29). */
  native?: NativeExecBinding;
}

/** A bridge kept alive across requests for tool result continuation. */
interface ActiveBridge {
  bridge: ReturnType<typeof spawnBridge>;
  heartbeatTimer: NodeJS.Timeout;
  blobStore: Map<string, Uint8Array>;
  mcpTools: McpToolDefinition[];
  cloudRule?: string;
  pendingExecs: PendingExec[];
}

// Active bridges keyed by a session token (derived from conversation state).
// When tool_calls are returned, the bridge stays alive. The next request
// with tool results looks up the bridge and sends mcpResult messages.
const activeBridges = new Map<string, ActiveBridge>();

interface StoredConversation {
  conversationId: string;
  checkpoint: Uint8Array | null;
  blobStore: Map<string, Uint8Array>;
  lastAccessMs: number;
}

const conversationStates = new Map<string, StoredConversation>();
const CONVERSATION_TTL_MS = 30 * 60 * 1000; // 30 minutes

// Conversation state also persists to disk so context survives proxy/opencode
// restarts instead of failing with "Blob not found" (issues #22/#29).
const CONVERSATION_DISK_TTL_MS = 7 * 24 * 60 * 60 * 1000; // 7 days
const CONVERSATION_DIR = pathResolve(
  process.env.XDG_CACHE_HOME ?? pathResolve(homedir(), ".cache"),
  "maki-cursor",
  "conversations",
);

const PersistedConversationSchema = z.object({
  conversationId: z.string(),
  checkpoint: z.string().nullable(),
  blobs: z.record(z.string()),
  lastAccessMs: z.number(),
});

/** Fire-and-forget write of a conversation snapshot to the disk cache. */
function persistConversation(convKey: string, stored: StoredConversation): void {
  const payload = {
    conversationId: stored.conversationId,
    checkpoint: stored.checkpoint
      ? Buffer.from(stored.checkpoint).toString("base64")
      : null,
    blobs: Object.fromEntries(
      [...stored.blobStore].map(([id, data]) => [
        id,
        Buffer.from(data).toString("base64"),
      ]),
    ),
    lastAccessMs: stored.lastAccessMs,
  } satisfies z.infer<typeof PersistedConversationSchema>;
  void mkdir(CONVERSATION_DIR, { recursive: true })
    .then(() =>
      writeFile(
        pathResolve(CONVERSATION_DIR, `${convKey}.json`),
        JSON.stringify(payload),
      ),
    )
    .catch(() => {});
}

async function loadPersistedConversation(
  convKey: string,
): Promise<StoredConversation | undefined> {
  try {
    const raw = await readFile(
      pathResolve(CONVERSATION_DIR, `${convKey}.json`),
      "utf8",
    );
    const parsed = PersistedConversationSchema.parse(JSON.parse(raw));
    if (Date.now() - parsed.lastAccessMs > CONVERSATION_DISK_TTL_MS) {
      return undefined;
    }
    return {
      conversationId: parsed.conversationId,
      checkpoint: parsed.checkpoint
        ? new Uint8Array(Buffer.from(parsed.checkpoint, "base64"))
        : null,
      blobStore: new Map(
        Object.entries(parsed.blobs).map(([id, data]) => [
          id,
          new Uint8Array(Buffer.from(data, "base64")),
        ]),
      ),
      lastAccessMs: Date.now(),
    };
  } catch {
    return undefined;
  }
}

/** Best-effort removal of conversation files past the disk TTL. */
function pruneStaleConversationFiles(): void {
  void (async () => {
    try {
      const entries = await readdir(CONVERSATION_DIR);
      const cutoff = Date.now() - CONVERSATION_DISK_TTL_MS;
      for (const entry of entries) {
        const file = pathResolve(CONVERSATION_DIR, entry);
        const info = await stat(file);
        if (info.mtimeMs < cutoff) await unlink(file);
      }
    } catch {}
  })();
}

function evictStaleConversations(): void {
  const now = Date.now();
  for (const [key, stored] of conversationStates) {
    if (now - stored.lastAccessMs > CONVERSATION_TTL_MS) {
      conversationStates.delete(key);
    }
  }
}

/** Length-prefix a message: [4-byte BE length][payload] */
function lpEncode(data: Uint8Array): Buffer {
  const buf = Buffer.alloc(4 + data.length);
  buf.writeUInt32BE(data.length, 0);
  buf.set(data, 4);
  return buf;
}

/** Connect protocol frame: [1-byte flags][4-byte BE length][payload] */
function frameConnectMessage(data: Uint8Array, flags = 0): Buffer {
  const frame = Buffer.alloc(5 + data.length);
  frame[0] = flags;
  frame.writeUInt32BE(data.length, 1);
  frame.set(data, 5);
  return frame;
}

/**
 * Spawn the Node H2 bridge and return read/write handles.
 * The bridge uses length-prefixed framing on stdin/stdout.
 */
interface SpawnBridgeOptions {
  accessToken: string;
  rpcPath: string;
  url?: string;
  /** When true, use application/proto for unary RPCs instead of Connect streaming. */
  unary?: boolean;
}

function spawnBridge(options: SpawnBridgeOptions): {
  proc: ChildProcess;
  write: (data: Uint8Array) => void;
  end: () => void;
  onData: (cb: (chunk: Buffer) => void) => void;
  onClose: (cb: (code: number) => void) => void;
  /** True while the bridge subprocess is still running. */
  get alive(): boolean;
} {
  const proc = spawn(process.execPath, [BRIDGE_PATH], {
    stdio: ["pipe", "pipe", "ignore"],
  });
  const stdin = proc.stdin;
  const stdout = proc.stdout;
  if (!stdin || !stdout) {
    throw new Error("failed to pipe the cursor bridge");
  }

  const config = JSON.stringify({
    accessToken: options.accessToken,
    url: options.url ?? CURSOR_API_URL,
    path: options.rpcPath,
    unary: options.unary ?? false,
  });
  stdin.write(lpEncode(new TextEncoder().encode(config)));

  const cbs = {
    data: null as ((chunk: Buffer) => void) | null,
    close: null as ((code: number) => void) | null,
  };
  const early: Buffer[] = [];

  let exited = false;
  let exitCode = 1;
  let pending = Buffer.alloc(0);

  const takeFrames = (value: Buffer) => {
    pending = Buffer.concat([pending, value]);
    while (pending.length >= 4) {
      const len = pending.readUInt32BE(0);
      if (pending.length < 4 + len) break;
      const payload = Buffer.from(pending.subarray(4, 4 + len));
      pending = pending.subarray(4 + len);
      if (cbs.data) cbs.data(payload);
      else early.push(payload);
    }
  };

  stdout.on("data", (value: Buffer) => takeFrames(value));
  const finish = (code: number) => {
    if (exited) return;
    exited = true;
    exitCode = code;
    cbs.close?.(code);
  };
  proc.on("exit", (code) => finish(code ?? 1));
  proc.on("error", () => finish(1));

  return {
    proc,
    get alive() {
      return !exited;
    },
    write(data) {
      try {
        stdin.write(lpEncode(data));
      } catch {
        // The bridge already closed.
      }
    },
    end() {
      try {
        stdin.write(lpEncode(new Uint8Array(0)));
        stdin.end();
      } catch {
        // The bridge already closed.
      }
    },
    onData(cb) {
      cbs.data = cb;
      for (const chunk of early) cb(chunk);
      early.length = 0;
    },
    onClose(cb) {
      if (exited) {
        queueMicrotask(() => cb(exitCode));
      } else {
        cbs.close = cb;
      }
    },
  };
}

interface CursorUnaryRpcOptions {
  accessToken: string;
  rpcPath: string;
  requestBody: Uint8Array;
  url?: string;
  timeoutMs?: number;
}

export async function callCursorUnaryRpc(
  options: CursorUnaryRpcOptions,
 ): Promise<{ body: Uint8Array; exitCode: number; timedOut: boolean }> {
  const bridge = spawnBridge({
    accessToken: options.accessToken,
    rpcPath: options.rpcPath,
    url: options.url,
    unary: true,
  });
  const chunks: Buffer[] = [];
  const { promise, resolve } = Promise.withResolvers<{
    body: Uint8Array;
    exitCode: number;
    timedOut: boolean;
  }>();
  let timedOut = false;
  const timeoutMs = options.timeoutMs ?? 5_000;
  const timeout = timeoutMs > 0
    ? setTimeout(() => {
        timedOut = true;
        try { bridge.proc.kill(); } catch {}
      }, timeoutMs)
    : undefined;

  bridge.onData((chunk) => {
    chunks.push(Buffer.from(chunk));
  });
  bridge.onClose((exitCode) => {
    if (timeout) clearTimeout(timeout);
    resolve({
      body: Buffer.concat(chunks),
      exitCode,
      timedOut,
    });
  });

  // Unary: send raw protobuf body (no Connect framing)
  bridge.write(options.requestBody);
  bridge.end();

  return promise;
}

let proxyServer: http.Server | undefined;
let proxyPort: number | undefined;
let proxyAccessTokenProvider: (() => Promise<string>) | undefined;
let proxyModels: Array<{ id: string; name: string }> = [];

function buildOpenAIModelList(models: ReadonlyArray<{ id: string; name: string }>): Array<{
  id: string;
  object: "model";
  created: number;
  owned_by: string;
}> {
  return models.map((model) => ({
    id: model.id,
    object: "model",
    created: 0,
    owned_by: "cursor",
  }));
}

export function getProxyPort(): number | undefined {
  return proxyPort;
}

/** Drop fields maki's OpenAI codec may add that Cursor's agent request does not understand. */
export function asCompletionRequest(value: unknown): ChatCompletionRequest {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("request body must be an object");
  }
  const record = { ...(value as Record<string, unknown>) };
  delete record.thinking;
  delete record.reasoning_effort;
  if (typeof record.model !== "string" || !Array.isArray(record.messages)) {
    throw new Error("model and messages are required");
  }
  return record as unknown as ChatCompletionRequest;
}

function headerPairs(headers: http.IncomingHttpHeaders): [string, string][] {
  const pairs: [string, string][] = [];
  for (const [key, value] of Object.entries(headers)) {
    if (value == null) continue;
    if (Array.isArray(value)) {
      for (const item of value) pairs.push([key, item]);
    } else {
      pairs.push([key, value]);
    }
  }
  return pairs;
}

async function writeNodeResponse(res: http.ServerResponse, response: Response): Promise<void> {
  const headers: Record<string, string> = {};
  response.headers.forEach((value, key) => {
    headers[key] = value;
  });
  res.writeHead(response.status, headers);
  if (!response.body) {
    res.end();
    return;
  }
  const reader = response.body.getReader();
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    res.write(Buffer.from(value));
  }
  res.end();
}

async function proxyFetch(req: Request): Promise<Response> {
  const url = new URL(req.url);
  if (req.method === "GET" && url.pathname === "/health") {
    return new Response("ok");
  }
  if (req.method === "GET" && url.pathname === "/v1/models") {
    return new Response(
      JSON.stringify({
        object: "list",
        data: buildOpenAIModelList(proxyModels),
      }),
      { headers: { "Content-Type": "application/json" } },
    );
  }
  if (req.method === "POST" && url.pathname === "/v1/chat/completions") {
    try {
      const body = asCompletionRequest(await req.json());
      if (!proxyAccessTokenProvider) {
        throw new Error("Cursor proxy access token provider not configured");
      }
      const accessToken = await proxyAccessTokenProvider();
      return handleChatCompletion(body, accessToken, req.headers.get("x-maki-session") ?? "");
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return new Response(
        JSON.stringify({
          error: { message, type: "server_error", code: "internal_error" },
        }),
        { status: 500, headers: { "Content-Type": "application/json" } },
      );
    }
  }
  return new Response("Not Found", { status: 404 });
}

export async function startProxy(
  getAccessToken: () => Promise<string>,
  models: ReadonlyArray<{ id: string; name: string }> = [],
  listenPort = 0,
): Promise<number> {
  proxyAccessTokenProvider = getAccessToken;
  proxyModels = models.map((model) => ({
    id: model.id,
    name: model.name,
  }));
  if (proxyServer && proxyPort) return proxyPort;

  pruneStaleConversationFiles();

  const server = http.createServer(async (req, res) => {
    try {
      const chunks: Buffer[] = [];
      for await (const chunk of req) chunks.push(Buffer.from(chunk));
      const body = Buffer.concat(chunks);
      const host = req.headers.host ?? "127.0.0.1";
      const request = new Request(`http://${host}${req.url ?? "/"}`, {
        method: req.method,
        headers: headerPairs(req.headers),
        body: req.method === "GET" || req.method === "HEAD" ? undefined : body,
      });
      await writeNodeResponse(res, await proxyFetch(request));
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      if (!res.headersSent) {
        res.writeHead(500, { "content-type": "application/json" });
      }
      res.end(JSON.stringify({ error: { message, type: "server_error" } }));
    }
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(listenPort, "127.0.0.1", () => resolve());
  });
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("Failed to bind proxy to a port");
  }
  proxyServer = server;
  proxyPort = address.port;
  return proxyPort;
}

export function stopProxy(): void {
  if (proxyServer) {
    proxyServer.close();
    proxyServer = undefined;
    proxyPort = undefined;
    proxyAccessTokenProvider = undefined;
    proxyModels = [];
  }
  for (const active of activeBridges.values()) {
    clearInterval(active.heartbeatTimer);
    active.bridge.end();
  }
  activeBridges.clear();
  conversationStates.clear();
}

async function handleChatCompletion(
  body: ChatCompletionRequest,
  accessToken: string,
  sessionId = "",
): Promise<Response> {
  const { systemPrompts, userText, history, toolResults } = parseMessages(body.messages);
  const modelId = body.model;
  const tools = body.tools ?? [];

  if (!userText && history.length === 0) {
    return new Response(
      JSON.stringify({
        error: {
          message: "No user message found",
          type: "invalid_request_error",
        },
      }),
      { status: 400, headers: { "Content-Type": "application/json" } },
    );
  }

  // bridgeKey: model-specific, for active tool-call bridges
  // convKey: model-independent, for conversation state that survives model switches
  const bridgeKey = deriveBridgeKey(modelId, body.messages, sessionId);
  const convKey = deriveConversationKey(body.messages, sessionId);
  const activeBridge = activeBridges.get(bridgeKey);

  if (activeBridge && toolResults.length > 0) {
    activeBridges.delete(bridgeKey);

    if (activeBridge.bridge.alive) {
      // Resume the live bridge with tool results
      return handleToolResultResume(activeBridge, toolResults, userText, modelId, bridgeKey, convKey);
    }

    // Bridge died (timeout, server disconnect, etc.).
    // Clean up and fall through to start a fresh bridge.
    clearInterval(activeBridge.heartbeatTimer);
    activeBridge.bridge.end();
  }

  // Clean up stale bridge if present
  if (activeBridge && activeBridges.has(bridgeKey)) {
    clearInterval(activeBridge.heartbeatTimer);
    activeBridge.bridge.end();
    activeBridges.delete(bridgeKey);
  }

  let stored = conversationStates.get(convKey);
  if (!stored) {
    // Fall back to the disk cache so conversations survive proxy restarts
    // and in-memory TTL eviction (issues #22/#29).
    stored = (await loadPersistedConversation(convKey)) ?? {
      conversationId: deterministicUuid(`cursor-conv-id:${convKey}`),
      checkpoint: null,
      blobStore: new Map(),
      lastAccessMs: Date.now(),
    };
    conversationStates.set(convKey, stored);
  }
  stored.lastAccessMs = Date.now();
  evictStaleConversations();

  // Build the request. When the bridge died mid tool-call, history already
  // contains the tool results and the request goes out as a resumeAction.
  const mcpTools = buildMcpToolDefinitions(tools);
  const payload = buildCursorRequest(
    modelId, systemPrompts, userText, history,
    stored.conversationId, stored.checkpoint, stored.blobStore,
  );
  payload.mcpTools = mcpTools;

  if (body.stream === false) {
    return handleNonStreamingResponse(payload, accessToken, modelId, convKey);
  }
  return handleStreamingResponse(payload, accessToken, modelId, bridgeKey, convKey);
}

interface ToolResultInfo {
  toolCallId: string;
  content: string;
}

/** One prior conversation event, in message order. */
type HistoryEntry =
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string }
  | { kind: "tool"; text: string };

interface ParsedMessages {
  systemPrompts: string[];
  /** Trailing user message that becomes the active action ("" = resume). */
  userText: string;
  /** Everything before the active user message, in order. */
  history: HistoryEntry[];
  toolResults: ToolResultInfo[];
}

/** Normalize OpenAI message content to a plain string. */
function textContent(content: OpenAIMessage["content"]): string {
  if (content == null) return "";
  if (typeof content === "string") return content;
  return content
    .filter((p) => p.type === "text" && p.text)
    .map((p) => p.text!)
    .join("\n");
}

function parseMessages(messages: OpenAIMessage[]): ParsedMessages {
  const systemPrompts = messages
    .filter((m) => m.role === "system")
    .map((m) => textContent(m.content))
    .filter((text) => text.length > 0);

  const toolResults: ToolResultInfo[] = [];
  const history: HistoryEntry[] = [];

  for (const msg of messages) {
    if (msg.role === "tool") {
      const content = textContent(msg.content);
      toolResults.push({
        toolCallId: msg.tool_call_id ?? "",
        content,
      });
      if (content) history.push({ kind: "tool", text: content });
    } else if (msg.role === "user") {
      history.push({ kind: "user", text: textContent(msg.content) });
    } else if (msg.role === "assistant") {
      // Pure tool_calls messages carry no text; the paired tool entries
      // preserve that part of the transcript.
      const text = textContent(msg.content);
      if (text) history.push({ kind: "assistant", text });
    }
  }

  // A trailing user message is the active action; anything else (e.g. tool
  // results after a dead bridge) leaves userText empty and the request is
  // sent as a resumeAction over the reconstructed history.
  let userText = "";
  const last = history[history.length - 1];
  if (last?.kind === "user") {
    userText = last.text;
    history.pop();
  }

  return { systemPrompts, userText, history, toolResults };
}

/** Convert OpenAI tool definitions to Cursor's MCP tool protobuf format. */
function buildMcpToolDefinitions(tools: OpenAIToolDef[]): McpToolDefinition[] {
  return tools.map((t) => {
    const fn = t.function;
    const jsonSchema: JsonValue =
      fn.parameters && typeof fn.parameters === "object"
        ? (fn.parameters as JsonValue)
        : { type: "object", properties: {}, required: [] };
    const inputSchema = toBinary(ValueSchema, fromJson(ValueSchema, jsonSchema));
    return create(McpToolDefinitionSchema, {
      name: fn.name,
      description: fn.description || "",
      providerIdentifier: "opencode",
      toolName: fn.name,
      inputSchema,
    });
  });
}

/** Decode a Cursor MCP arg value (protobuf Value bytes) to a JS value. */
function decodeMcpArgValue(value: Uint8Array): unknown {
  try {
    const parsed = fromBinary(ValueSchema, value);
    return toJson(ValueSchema, parsed);
  } catch {}
  return new TextDecoder().decode(value);
}

/** Decode a map of MCP arg values. */
function decodeMcpArgsMap(args: Record<string, Uint8Array>): Record<string, unknown> {
  const decoded: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(args)) {
    decoded[key] = decodeMcpArgValue(value);
  }
  return decoded;
}

function buildCursorRequest(
  modelId: string,
  systemPrompts: string[],
  userText: string,
  history: HistoryEntry[],
  conversationId: string,
  checkpoint: Uint8Array | null,
  existingBlobStore?: Map<string, Uint8Array>,
): CursorRequestPayload {
  const blobStore = new Map<string, Uint8Array>(existingBlobStore ?? []);

  // Every `bytes` field in the *Structure messages is a sha256 blob ID —
  // server-produced checkpoints content-address turns, user messages, and
  // steps alike. Inlining data where an ID is expected makes Cursor fail
  // with "Connect error internal: Blob not found" (issues #22/#29).
  const storeBlob = (bytes: Uint8Array): Uint8Array => {
    const blobId = new Uint8Array(createHash("sha256").update(bytes).digest());
    blobStore.set(Buffer.from(blobId).toString("hex"), bytes);
    return blobId;
  };
  const storeJsonBlob = (obj: unknown): Uint8Array =>
    storeBlob(new TextEncoder().encode(JSON.stringify(obj)));

  const prompts = systemPrompts.length > 0
    ? systemPrompts
    : ["You are a helpful assistant."];
  const systemBlobIds = prompts.map((content) =>
    storeJsonBlob({ role: "system", content }),
  );

  // Cursor's server builds the model prompt from `rootPromptMessagesJson`,
  // not from `turns[]`. Sending only the system prompt here makes multi-turn
  // conversations lose all prior context after a proxy restart, so the full
  // history is rebuilt on every request. Server-echoed checkpoints replace
  // historical user entries with empty placeholders, which is why the
  // checkpoint's own rootPromptMessagesJson cannot be reused.
  const rootPromptMessagesJson = [...systemBlobIds];
  for (const entry of history) {
    if (entry.kind === "assistant") {
      rootPromptMessagesJson.push(
        storeJsonBlob({ role: "assistant", content: [{ type: "text", text: entry.text }] }),
      );
    } else {
      const text = entry.kind === "tool" ? `[Tool Result]\n${entry.text}` : entry.text;
      rootPromptMessagesJson.push(
        storeJsonBlob({ role: "user", content: [{ type: "text", text }] }),
      );
    }
  }

  // turns[]: one entry per user turn, with assistant/tool texts as steps.
  // Deterministic message IDs keep blob IDs stable across rebuilds.
  const turnBlobIds: Uint8Array[] = [];
  let currentTurn: { userMessageBlobId: Uint8Array; stepBlobIds: Uint8Array[] } | null = null;
  const flushTurn = () => {
    if (!currentTurn) return;
    const agentTurn = create(AgentConversationTurnStructureSchema, {
      userMessage: currentTurn.userMessageBlobId,
      steps: currentTurn.stepBlobIds,
    });
    const turnStructure = create(ConversationTurnStructureSchema, {
      turn: { case: "agentConversationTurn", value: agentTurn },
    });
    turnBlobIds.push(storeBlob(toBinary(ConversationTurnStructureSchema, turnStructure)));
    currentTurn = null;
  };
  for (const entry of history) {
    if (entry.kind === "user") {
      flushTurn();
      const userMsg = create(UserMessageSchema, {
        text: entry.text,
        messageId: deterministicUuid(`u:${turnBlobIds.length}:${entry.text}`),
      });
      currentTurn = {
        userMessageBlobId: storeBlob(toBinary(UserMessageSchema, userMsg)),
        stepBlobIds: [],
      };
    } else if (currentTurn) {
      const text = entry.kind === "tool" ? `[Tool Result]\n${entry.text}` : entry.text;
      const step = create(ConversationStepSchema, {
        message: {
          case: "assistantMessage",
          value: create(AssistantMessageSchema, { text }),
        },
      });
      currentTurn.stepBlobIds.push(storeBlob(toBinary(ConversationStepSchema, step)));
    }
  }
  flushTurn();

  // Preserve non-history checkpoint fields (todos, file states, summaries)
  // when the system prompt is unchanged; otherwise start fresh.
  let baseState: ConversationStateStructure | null = null;
  if (checkpoint) {
    try {
      const decoded = fromBinary(ConversationStateStructureSchema, checkpoint);
      const head = decoded.rootPromptMessagesJson.slice(0, systemBlobIds.length);
      const matches =
        head.length === systemBlobIds.length &&
        systemBlobIds.every((id, idx) => Buffer.from(head[idx]!).equals(Buffer.from(id)));
      if (matches) baseState = decoded;
    } catch {}
  }

  const conversationState = baseState
    ? create(ConversationStateStructureSchema, {
        ...baseState,
        rootPromptMessagesJson,
        turns: turnBlobIds,
      })
    : create(ConversationStateStructureSchema, {
        rootPromptMessagesJson,
        turns: turnBlobIds,
        todos: [],
        pendingToolCalls: [],
        previousWorkspaceUris: [],
        fileStates: {},
        fileStatesV2: {},
        summaryArchives: [],
        turnTimings: [],
        subagentStates: {},
        selfSummaryCount: 0,
        readPaths: [],
      });

  // No trailing user message (e.g. tool results after a dead bridge) →
  // resume over the reconstructed history instead of faking a user turn.
  const action = userText
    ? create(ConversationActionSchema, {
        action: {
          case: "userMessageAction",
          value: create(UserMessageActionSchema, {
            userMessage: create(UserMessageSchema, {
              text: userText,
              messageId: crypto.randomUUID(),
            }),
          }),
        },
      })
    : create(ConversationActionSchema, {
        action: { case: "resumeAction", value: create(ResumeActionSchema, {}) },
      });

  // "auto" is the proxy's pseudo-model for Cursor's server-side Auto
  // routing; the Run API expects modelId "default" for it.
  const cursorModelId = modelId === "auto" ? "default" : modelId;
  const displayName = modelId === "auto" ? "Auto" : modelId;
  const requestedModel = create(RequestedModelSchema, {
    modelId: cursorModelId,
  });
  const modelDetails = create(ModelDetailsSchema, {
    modelId: cursorModelId,
    displayModelId: cursorModelId,
    displayName,
    displayNameShort: displayName,
  });

  const runRequest = create(AgentRunRequestSchema, {
    conversationState,
    action,
    modelDetails,
    requestedModel,
    conversationId,
  });

  const clientMessage = create(AgentClientMessageSchema, {
    message: { case: "runRequest", value: runRequest },
  });

  return {
    requestBytes: toBinary(AgentClientMessageSchema, clientMessage),
    blobStore,
    mcpTools: [],
    cloudRule: prompts.join("\n\n").trim() || undefined,
  };
}

function parseConnectEndStream(data: Uint8Array): Error | null {
  try {
    const payload = JSON.parse(new TextDecoder().decode(data));
    const error = payload?.error;
    if (error) {
      const code = error.code ?? "unknown";
      const message = error.message ?? "Unknown error";
      return new Error(`Connect error ${code}: ${message}`);
    }
    return null;
  } catch {
    return new Error("Failed to parse Connect end stream");
  }
}

function makeHeartbeatBytes(): Uint8Array {
  const heartbeat = create(AgentClientMessageSchema, {
    message: {
      case: "clientHeartbeat",
      value: create(ClientHeartbeatSchema, {}),
    },
  });
  return frameConnectMessage(toBinary(AgentClientMessageSchema, heartbeat));
}

/**
 * Create a stateful parser for Connect protocol frames.
 * Handles buffering partial data across chunks.
 */
function createConnectFrameParser(
  onMessage: (bytes: Uint8Array) => void,
  onEndStream: (bytes: Uint8Array) => void,
): (incoming: Buffer) => void {
  let pending = Buffer.alloc(0);
  return (incoming: Buffer) => {
    pending = Buffer.concat([pending, incoming]);
    while (pending.length >= 5) {
      const flags = pending[0]!;
      const msgLen = pending.readUInt32BE(1);
      if (pending.length < 5 + msgLen) break;
      const messageBytes = pending.subarray(5, 5 + msgLen);
      pending = pending.subarray(5 + msgLen);
      if (flags & CONNECT_END_STREAM_FLAG) {
        onEndStream(messageBytes);
      } else {
        onMessage(messageBytes);
      }
    }
  };
}

const THINKING_TAG_NAMES = ['think', 'thinking', 'reasoning', 'thought', 'think_intent'];
const MAX_THINKING_TAG_LEN = 16; // </think_intent> is 15 chars

/**
 * Strip thinking tags from streamed text, routing tagged content to reasoning.
 * Buffers partial tags across chunk boundaries.
 */
function createThinkingTagFilter(): {
  process(text: string): { content: string; reasoning: string };
  flush(): { content: string; reasoning: string };
} {
  let buffer = '';
  let inThinking = false;

  return {
    process(text: string) {
      const input = buffer + text;
      buffer = '';
      let content = '';
      let reasoning = '';
      let lastIdx = 0;

      const re = new RegExp(`<(/?)(?:${THINKING_TAG_NAMES.join('|')})\\s*>`, 'gi');
      let match: RegExpExecArray | null;
      while ((match = re.exec(input)) !== null) {
        const before = input.slice(lastIdx, match.index);
        if (inThinking) reasoning += before;
        else content += before;
        inThinking = match[1] !== '/';
        lastIdx = re.lastIndex;
      }

      const rest = input.slice(lastIdx);
      // Buffer a trailing '<' that could be the start of a thinking tag.
      const ltPos = rest.lastIndexOf('<');
      if (ltPos >= 0 && rest.length - ltPos < MAX_THINKING_TAG_LEN && /^<\/?[a-z_]*$/i.test(rest.slice(ltPos))) {
        buffer = rest.slice(ltPos);
        const before = rest.slice(0, ltPos);
        if (inThinking) reasoning += before;
        else content += before;
      } else {
        if (inThinking) reasoning += rest;
        else content += rest;
      }

      return { content, reasoning };
    },
    flush() {
      const b = buffer;
      buffer = '';
      if (!b) return { content: '', reasoning: '' };
      return inThinking ? { content: '', reasoning: b } : { content: b, reasoning: '' };
    },
  };
}

interface StreamState {
  toolCallIndex: number;
  pendingExecs: PendingExec[];
  outputTokens: number;
  totalTokens: number;
}

function computeUsage(state: StreamState) {
  const completion_tokens = state.outputTokens;
  const total_tokens = state.totalTokens || completion_tokens;
  const prompt_tokens = Math.max(0, total_tokens - completion_tokens);
  return { prompt_tokens, completion_tokens, total_tokens };
}

function processServerMessage(
  msg: AgentServerMessage,
  blobStore: Map<string, Uint8Array>,
  mcpTools: McpToolDefinition[],
  cloudRule: string | undefined,
  sendFrame: (data: Uint8Array) => void,
  state: StreamState,
  onText: (text: string, isThinking?: boolean) => void,
  onMcpExec: (exec: PendingExec) => void,
  onCheckpoint?: (checkpointBytes: Uint8Array) => void,
): void {
  const msgCase = msg.message.case;

  if (msgCase === "interactionUpdate") {
    handleInteractionUpdate(msg.message.value, state, onText);
  } else if (msgCase === "kvServerMessage") {
    handleKvMessage(msg.message.value as KvServerMessage, blobStore, sendFrame);
  } else if (msgCase === "execServerMessage") {
    handleExecMessage(
      msg.message.value as ExecServerMessage,
      mcpTools,
      cloudRule,
      sendFrame,
      onMcpExec,
    );
  } else if (msgCase === "conversationCheckpointUpdate") {
    const stateStructure = msg.message.value as ConversationStateStructure;
    if (stateStructure.tokenDetails) {
      state.totalTokens = stateStructure.tokenDetails.usedTokens;
    }
    if (onCheckpoint) {
      onCheckpoint(toBinary(ConversationStateStructureSchema, stateStructure));
    }
  }
}

function handleInteractionUpdate(
  update: any,
  state: StreamState,
  onText: (text: string, isThinking?: boolean) => void,
): void {
  const updateCase = update.message?.case;

  if (updateCase === "textDelta") {
    const delta = update.message.value.text || "";
    if (delta) onText(delta, false);
  } else if (updateCase === "thinkingDelta") {
    const delta = update.message.value.text || "";
    if (delta) onText(delta, true);
  } else if (updateCase === "tokenDelta") {
    state.outputTokens += update.message.value.tokens ?? 0;
  }
  // toolCallStarted, partialToolCall, toolCallDelta, toolCallCompleted
  // are intentionally ignored. MCP tool calls flow through the exec
  // message path (mcpArgs → mcpResult), not interaction updates.
}

/** Send a KV client response back to Cursor. */
function sendKvResponse(
  kvMsg: KvServerMessage,
  messageCase: string,
  value: unknown,
  sendFrame: (data: Uint8Array) => void,
): void {
  const response = create(KvClientMessageSchema, {
    id: kvMsg.id,
    message: { case: messageCase as any, value: value as any },
  });
  const clientMsg = create(AgentClientMessageSchema, {
    message: { case: "kvClientMessage", value: response },
  });
  sendFrame(frameConnectMessage(toBinary(AgentClientMessageSchema, clientMsg)));
}

function handleKvMessage(
  kvMsg: KvServerMessage,
  blobStore: Map<string, Uint8Array>,
  sendFrame: (data: Uint8Array) => void,
): void {
  const kvCase = kvMsg.message.case;

  if (kvCase === "getBlobArgs") {
    const blobId = kvMsg.message.value.blobId;
    const blobIdKey = Buffer.from(blobId).toString("hex");
    const blobData = blobStore.get(blobIdKey);
    if (process.env.CURSOR_PROXY_DEBUG) {
      console.error(`[proxy] getBlob ${blobIdKey.slice(0, 16)} ${blobData ? `hit (${blobData.length}b)` : "MISS"}`);
    }
    sendKvResponse(
      kvMsg, "getBlobResult",
      create(GetBlobResultSchema, blobData ? { blobData } : {}),
      sendFrame,
    );
  } else if (kvCase === "setBlobArgs") {
    const { blobId, blobData } = kvMsg.message.value;
    blobStore.set(Buffer.from(blobId).toString("hex"), blobData);
    if (process.env.CURSOR_PROXY_DEBUG) {
      console.error(`[proxy] setBlob ${Buffer.from(blobId).toString("hex").slice(0, 16)} (${blobData.length}b)`);
    }
    sendKvResponse(
      kvMsg, "setBlobResult",
      create(SetBlobResultSchema, {}),
      sendFrame,
    );
  }
}

function handleExecMessage(
  execMsg: ExecServerMessage,
  mcpTools: McpToolDefinition[],
  cloudRule: string | undefined,
  sendFrame: (data: Uint8Array) => void,
  onMcpExec: (exec: PendingExec) => void,
): void {
  const execCase = execMsg.message.case;
  if (process.env.CURSOR_PROXY_DEBUG) {
    console.error(`[proxy] exec: ${execCase}`);
  }

  if (execCase === "requestContextArgs") {
    // cloudRule is the prompt channel Cursor's agent actually honors; plain
    // system messages are ignored server-side (issue #21).
    const requestContext = create(RequestContextSchema, {
      rules: [],
      cloudRule,
      repositoryInfo: [],
      tools: mcpTools,
      gitRepos: [],
      projectLayouts: [],
      mcpInstructions: [],
      fileContents: {},
      customSubagents: [],
    });
    const result = create(RequestContextResultSchema, {
      result: {
        case: "success",
        value: create(RequestContextSuccessSchema, { requestContext }),
      },
    });
    sendExecResult(execMsg, "requestContextResult", result, sendFrame);
    return;
  }

  if (execCase === "mcpArgs") {
    const mcpArgs = execMsg.message.value;
    const decoded = decodeMcpArgsMap(mcpArgs.args ?? {});
    onMcpExec({
      execId: execMsg.execId,
      execMsgId: execMsg.id,
      toolCallId: mcpArgs.toolCallId || crypto.randomUUID(),
      toolName: mcpArgs.toolName || mcpArgs.name,
      decodedArgs: JSON.stringify(decoded),
    });
    return;
  }

  // --- Native Cursor tools ---
  // The model tries these before the MCP tools. When the client provides an
  // equivalent tool, redirect the call to it; otherwise reject so the model
  // falls back to the MCP tools registered via RequestContext.
  const redirect = redirectNativeExec(execMsg, mcpTools);
  if (redirect) {
    if (process.env.CURSOR_PROXY_DEBUG) {
      console.error(`[proxy] redirect ${execCase} -> ${redirect.toolName}`);
    }
    onMcpExec({
      execId: execMsg.execId,
      execMsgId: execMsg.id,
      toolCallId: redirect.toolCallId,
      toolName: redirect.toolName,
      decodedArgs: redirect.decodedArgs,
      native: redirect.binding,
    });
    return;
  }

  const REJECT_REASON = "Tool not available in this environment. Use the MCP tools provided instead.";

  if (execCase === "readArgs") {
    const args = execMsg.message.value;
    const result = create(ReadResultSchema, {
      result: { case: "rejected", value: create(ReadRejectedSchema, { path: args.path, reason: REJECT_REASON }) },
    });
    sendExecResult(execMsg, "readResult", result, sendFrame);
    return;
  }
  if (execCase === "lsArgs") {
    const args = execMsg.message.value;
    const result = create(LsResultSchema, {
      result: { case: "rejected", value: create(LsRejectedSchema, { path: args.path, reason: REJECT_REASON }) },
    });
    sendExecResult(execMsg, "lsResult", result, sendFrame);
    return;
  }
  if (execCase === "grepArgs") {
    const result = create(GrepResultSchema, {
      result: { case: "error", value: create(GrepErrorSchema, { error: REJECT_REASON }) },
    });
    sendExecResult(execMsg, "grepResult", result, sendFrame);
    return;
  }
  if (execCase === "writeArgs") {
    const args = execMsg.message.value;
    const result = create(WriteResultSchema, {
      result: { case: "rejected", value: create(WriteRejectedSchema, { path: args.path, reason: REJECT_REASON }) },
    });
    sendExecResult(execMsg, "writeResult", result, sendFrame);
    return;
  }
  if (execCase === "deleteArgs") {
    const args = execMsg.message.value;
    const result = create(DeleteResultSchema, {
      result: { case: "rejected", value: create(DeleteRejectedSchema, { path: args.path, reason: REJECT_REASON }) },
    });
    sendExecResult(execMsg, "deleteResult", result, sendFrame);
    return;
  }
  if (execCase === "shellArgs" || execCase === "shellStreamArgs") {
    const args = execMsg.message.value;
    const result = create(ShellResultSchema, {
      result: {
        case: "rejected",
        value: create(ShellRejectedSchema, {
          command: args.command ?? "",
          workingDirectory: args.workingDirectory ?? "",
          reason: REJECT_REASON,
          isReadonly: false,
        }),
      },
    });
    sendExecResult(execMsg, "shellResult", result, sendFrame);
    return;
  }
  if (execCase === "backgroundShellSpawnArgs") {
    const args = execMsg.message.value;
    const result = create(BackgroundShellSpawnResultSchema, {
      result: {
        case: "rejected",
        value: create(ShellRejectedSchema, {
          command: args.command ?? "",
          workingDirectory: args.workingDirectory ?? "",
          reason: REJECT_REASON,
          isReadonly: false,
        }),
      },
    });
    sendExecResult(execMsg, "backgroundShellSpawnResult", result, sendFrame);
    return;
  }
  if (execCase === "writeShellStdinArgs") {
    const result = create(WriteShellStdinResultSchema, {
      result: { case: "error", value: create(WriteShellStdinErrorSchema, { error: REJECT_REASON }) },
    });
    sendExecResult(execMsg, "writeShellStdinResult", result, sendFrame);
    return;
  }
  if (execCase === "fetchArgs") {
    const args = execMsg.message.value;
    const result = create(FetchResultSchema, {
      result: { case: "error", value: create(FetchErrorSchema, { url: args.url ?? "", error: REJECT_REASON }) },
    });
    sendExecResult(execMsg, "fetchResult", result, sendFrame);
    return;
  }
  if (execCase === "diagnosticsArgs") {
    const result = create(DiagnosticsResultSchema, {});
    sendExecResult(execMsg, "diagnosticsResult", result, sendFrame);
    return;
  }

  // MCP resource/screen/computer exec types
  const miscCaseMap: Record<string, string> = {
    listMcpResourcesExecArgs: "listMcpResourcesExecResult",
    readMcpResourceExecArgs: "readMcpResourceExecResult",
    recordScreenArgs: "recordScreenResult",
    computerUseArgs: "computerUseResult",
  };
  const resultCase = miscCaseMap[execCase as string];
  if (resultCase) {
    sendExecResult(execMsg, resultCase, create(McpResultSchema, {}), sendFrame);
    return;
  }

  // Unknown exec type — log and ignore
  console.error(`[proxy] unhandled exec: ${execCase}`);
}

/** Send an exec client message back to Cursor. */
function sendExecResult(
  execMsg: ExecServerMessage,
  messageCase: string,
  value: unknown,
  sendFrame: (data: Uint8Array) => void,
): void {
  const execClientMessage = create(ExecClientMessageSchema, {
    id: execMsg.id,
    execId: execMsg.execId,
    message: { case: messageCase as any, value: value as any },
  });
  const clientMessage = create(AgentClientMessageSchema, {
    message: { case: "execClientMessage", value: execClientMessage },
  });
  sendFrame(frameConnectMessage(toBinary(AgentClientMessageSchema, clientMessage)));
}

/** Deterministic v4-shaped UUID from a seed (first 16 bytes of SHA-256).
 *  Keeps conversation and message IDs stable across proxy restarts so
 *  Cursor's server-side caches stay warm. */
function deterministicUuid(seed: string): string {
  const hex = createHash("sha256").update(seed).digest("hex").slice(0, 32);
  // Format as UUID: xxxxxxxx-xxxx-4xxx-Nxxx-xxxxxxxxxxxx
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    `4${hex.slice(13, 16)}`,
    `${(0x8 | (parseInt(hex[16], 16) & 0x3)).toString(16)}${hex.slice(17, 20)}`,
    hex.slice(20, 32),
  ].join("-");
}

/** Create an SSE streaming Response that reads from a live bridge. */
function createBridgeStreamResponse(
  bridge: ReturnType<typeof spawnBridge>,
  heartbeatTimer: NodeJS.Timeout,
  blobStore: Map<string, Uint8Array>,
  mcpTools: McpToolDefinition[],
  cloudRule: string | undefined,
  modelId: string,
  bridgeKey: string,
  convKey: string,
): Response {
  const completionId = `chatcmpl-${crypto.randomUUID().replace(/-/g, "").slice(0, 28)}`;
  const created = Math.floor(Date.now() / 1000);

  const stream = new ReadableStream({
    start(controller) {
      const encoder = new TextEncoder();
      let closed = false;
      const sendSSE = (data: object) => {
        if (closed) return;
        controller.enqueue(encoder.encode(`data: ${JSON.stringify(data)}\n\n`));
      };
      const sendDone = () => {
        if (closed) return;
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
      };
      const closeController = () => {
        if (closed) return;
        closed = true;
        controller.close();
      };

      const makeChunk = (
        delta: Record<string, unknown>,
        finishReason: string | null = null,
      ) => ({
        id: completionId,
        object: "chat.completion.chunk",
        created,
        model: modelId,
        choices: [{ index: 0, delta, finish_reason: finishReason }],
      });

      const makeUsageChunk = () => {
        const { prompt_tokens, completion_tokens, total_tokens } = computeUsage(state);
        return {
          id: completionId,
          object: "chat.completion.chunk",
          created,
          model: modelId,
          choices: [],
          usage: { prompt_tokens, completion_tokens, total_tokens },
        };
      };

      const state: StreamState = {
        toolCallIndex: 0,
        pendingExecs: [],
        outputTokens: 0,
        totalTokens: 0,
      };
      const tagFilter = createThinkingTagFilter();

      let mcpExecReceived = false;

      const processChunk = createConnectFrameParser(
        (messageBytes) => {
          try {
            const serverMessage = fromBinary(
              AgentServerMessageSchema,
              messageBytes,
            );
            processServerMessage(
              serverMessage,
              blobStore,
              mcpTools,
              cloudRule,
              (data) => bridge.write(data),
              state,
              (text, isThinking) => {
                if (isThinking) {
                  sendSSE(makeChunk({ reasoning_content: text }));
                } else {
                  const { content, reasoning } = tagFilter.process(text);
                  if (reasoning) sendSSE(makeChunk({ reasoning_content: reasoning }));
                  if (content) sendSSE(makeChunk({ content }));
                }
              },
              // onMcpExec — the model wants to execute a tool.
              (exec) => {
                state.pendingExecs.push(exec);
                mcpExecReceived = true;

                const flushed = tagFilter.flush();
                if (flushed.reasoning) sendSSE(makeChunk({ reasoning_content: flushed.reasoning }));
                if (flushed.content) sendSSE(makeChunk({ content: flushed.content }));

                const toolCallIndex = state.toolCallIndex++;
                sendSSE(makeChunk({
                  tool_calls: [{
                    index: toolCallIndex,
                    id: exec.toolCallId,
                    type: "function",
                    function: {
                      name: exec.toolName,
                      arguments: exec.decodedArgs,
                    },
                  }],
                }));

                // Keep the bridge alive for tool result continuation.
                activeBridges.set(bridgeKey, {
                  bridge,
                  heartbeatTimer,
                  blobStore,
                  mcpTools,
                  cloudRule,
                  pendingExecs: state.pendingExecs,
                });

                sendSSE(makeChunk({}, "tool_calls"));
                sendDone();
                closeController();
              },
              (checkpointBytes) => {
                const stored = conversationStates.get(convKey);
                if (stored) {
                  stored.checkpoint = checkpointBytes;
                  // Merge live blobs before persisting: the checkpoint may
                  // reference blobs set during this stream.
                  for (const [k, v] of blobStore) stored.blobStore.set(k, v);
                  stored.lastAccessMs = Date.now();
                  persistConversation(convKey, stored);
                }
              },
            );
          } catch {
            // Skip unparseable messages
          }
        },
        (endStreamBytes) => {
          const endError = parseConnectEndStream(endStreamBytes);
          if (process.env.CURSOR_PROXY_DEBUG) {
            console.error(`[proxy] endStream: ${endError ? endError.message : "clean"}`);
          }
          if (endError) {
            // Surface the error and shut down: the server is done with this
            // stream, and heartbeats would otherwise keep the bridge (and the
            // SSE response) open forever.
            sendSSE(makeChunk({ content: `\n[Error: ${endError.message}]` }));
            sendSSE(makeChunk({}, "stop"));
            sendSSE(makeUsageChunk());
            sendDone();
            closeController();
            activeBridges.delete(bridgeKey);
            clearInterval(heartbeatTimer);
            bridge.end();
          }
        },
      );

      bridge.onData(processChunk);

      bridge.onClose((code) => {
        clearInterval(heartbeatTimer);
        const stored = conversationStates.get(convKey);
        if (stored) {
          for (const [k, v] of blobStore) stored.blobStore.set(k, v);
          stored.lastAccessMs = Date.now();
          persistConversation(convKey, stored);
        }
        if (!mcpExecReceived) {
          const flushed = tagFilter.flush();
          if (flushed.reasoning) sendSSE(makeChunk({ reasoning_content: flushed.reasoning }));
          if (flushed.content) sendSSE(makeChunk({ content: flushed.content }));
          sendSSE(makeChunk({}, "stop"));
          sendSSE(makeUsageChunk());
          sendDone();
          closeController();
        } else if (code !== 0) {
          // Bridge died while tool calls are pending (timeout, crash, etc.).
          // Close the SSE stream so the client doesn't hang forever.
          sendSSE(makeChunk({ content: "\n[Error: bridge connection lost]" }));
          sendSSE(makeChunk({}, "stop"));
          sendSSE(makeUsageChunk());
          sendDone();
          closeController();
          // Remove stale entry so the next request doesn't try to resume it.
          activeBridges.delete(bridgeKey);
        }
      });
    },
  });

  return new Response(stream, { headers: SSE_HEADERS });
}

/** Spawn a bridge, send the initial request frame, and start heartbeat. */
function startBridge(
  accessToken: string,
  requestBytes: Uint8Array,
): { bridge: ReturnType<typeof spawnBridge>; heartbeatTimer: NodeJS.Timeout } {
  const bridge = spawnBridge({
    accessToken,
    rpcPath: "/agent.v1.AgentService/Run",
  });
  bridge.write(frameConnectMessage(requestBytes));
  const heartbeatTimer = setInterval(() => bridge.write(makeHeartbeatBytes()), 5_000);
  return { bridge, heartbeatTimer };
}

function handleStreamingResponse(
  payload: CursorRequestPayload,
  accessToken: string,
  modelId: string,
  bridgeKey: string,
  convKey: string,
): Response {
  const { bridge, heartbeatTimer } = startBridge(accessToken, payload.requestBytes);
  return createBridgeStreamResponse(
    bridge, heartbeatTimer,
    payload.blobStore, payload.mcpTools, payload.cloudRule,
    modelId, bridgeKey, convKey,
  );
}

/** Resume a paused bridge by sending MCP results and continuing to stream. */
function handleToolResultResume(
  active: ActiveBridge,
  toolResults: ToolResultInfo[],
  userText: string,
  modelId: string,
  bridgeKey: string,
  convKey: string,
): Response {
  const { bridge, heartbeatTimer, blobStore, mcpTools, cloudRule, pendingExecs } = active;

  // Answer each pending exec with a matching tool result: redirected native
  // execs get their typed native result frame, MCP execs get an mcpResult.
  const lastExecId = pendingExecs[pendingExecs.length - 1]?.execId;
  for (const exec of pendingExecs) {
    const result = toolResults.find(
      (r) => r.toolCallId === exec.toolCallId,
    );
    // A user message sent alongside tool results (e.g. the explanation typed
    // after rejecting an edit) would otherwise be dropped: the paused bridge
    // only accepts tool results. Attach it to the last result so the model
    // sees it (issue #23).
    let text = result ? result.content : "Tool result not provided";
    if (userText && exec.execId === lastExecId) {
      text += `\n\n<user_message>\n${userText}\n</user_message>`;
    }

    if (result && exec.native) {
      const sent = sendNativeExecResult(exec, exec.native, text, (bytes) =>
        bridge.write(frameConnectMessage(bytes)),
      );
      if (sent) continue;
    }

    const mcpResult = result
      ? create(McpResultSchema, {
          result: {
            case: "success",
            value: create(McpSuccessSchema, {
              content: [
                create(McpToolResultContentItemSchema, {
                  content: {
                    case: "text",
                    value: create(McpTextContentSchema, { text }),
                  },
                }),
              ],
              isError: false,
            }),
          },
        })
      : create(McpResultSchema, {
          result: {
            case: "error",
            value: create(McpErrorSchema, { error: text }),
          },
        });

    const execClientMessage = create(ExecClientMessageSchema, {
      id: exec.execMsgId,
      execId: exec.execId,
      message: {
        case: "mcpResult" as never,
        value: mcpResult as never,
      },
    });

    const clientMessage = create(AgentClientMessageSchema, {
      message: { case: "execClientMessage", value: execClientMessage },
    });

    bridge.write(
      frameConnectMessage(toBinary(AgentClientMessageSchema, clientMessage)),
    );
  }

  return createBridgeStreamResponse(
    bridge, heartbeatTimer,
    blobStore, mcpTools, cloudRule,
    modelId, bridgeKey, convKey,
  );
}

async function handleNonStreamingResponse(
  payload: CursorRequestPayload,
  accessToken: string,
  modelId: string,
  convKey: string,
): Promise<Response> {
  const completionId = `chatcmpl-${crypto.randomUUID().replace(/-/g, "").slice(0, 28)}`;
  const created = Math.floor(Date.now() / 1000);
  const { text, usage } = await collectFullResponse(payload, accessToken, convKey);

  return new Response(
    JSON.stringify({
      id: completionId,
      object: "chat.completion",
      created,
      model: modelId,
      choices: [
        {
          index: 0,
          message: { role: "assistant", content: text },
          finish_reason: "stop",
        },
      ],
      usage,
    }),
    { headers: { "Content-Type": "application/json" } },
  );
}

interface CollectedResponse {
  text: string;
  usage: { prompt_tokens: number; completion_tokens: number; total_tokens: number };
}

async function collectFullResponse(
  payload: CursorRequestPayload,
  accessToken: string,
  convKey: string,
): Promise<CollectedResponse> {
  const { promise, resolve } = Promise.withResolvers<CollectedResponse>();
  let fullText = "";

  const { bridge, heartbeatTimer } = startBridge(accessToken, payload.requestBytes);

  const state: StreamState = {
    toolCallIndex: 0,
    pendingExecs: [],
    outputTokens: 0,
    totalTokens: 0,
  };
  const tagFilter = createThinkingTagFilter();

  bridge.onData(createConnectFrameParser(
    (messageBytes) => {
      try {
        const serverMessage = fromBinary(
          AgentServerMessageSchema,
          messageBytes,
        );
        processServerMessage(
          serverMessage,
          payload.blobStore,
          payload.mcpTools,
          payload.cloudRule,
          (data) => bridge.write(data),
          state,
          (text, isThinking) => {
            if (isThinking) return;
            const { content } = tagFilter.process(text);
            fullText += content;
          },
          () => {},
          (checkpointBytes) => {
            const stored = conversationStates.get(convKey);
            if (stored) {
              stored.checkpoint = checkpointBytes;
              for (const [k, v] of payload.blobStore) stored.blobStore.set(k, v);
              stored.lastAccessMs = Date.now();
              persistConversation(convKey, stored);
            }
          },
        );
      } catch {
        // Skip
      }
    },
    () => {},
  ));

  bridge.onClose(() => {
    clearInterval(heartbeatTimer);
    const stored = conversationStates.get(convKey);
    if (stored) {
      for (const [k, v] of payload.blobStore) stored.blobStore.set(k, v);
      stored.lastAccessMs = Date.now();
      persistConversation(convKey, stored);
    }
    const flushed = tagFilter.flush();
    fullText += flushed.content;

    const usage = computeUsage(state);
    resolve({
      text: fullText,
      usage,
    });
  });

  return promise;
}
