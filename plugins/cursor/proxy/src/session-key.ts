import { createHash } from "node:crypto";

/** A message shape both the proxy and tests can hash without pulling in protobuf. */
export interface KeyMessage {
  role: string;
  content?: unknown;
  tool_call_id?: string;
  tool_calls?: Array<{ id?: string; function?: { name?: string } }>;
}

function textOf(content: unknown): string {
  if (typeof content === "string") return content;
  if (content == null) return "";
  if (!Array.isArray(content)) return "";
  return content
    .map((part) => {
      if (part && typeof part === "object" && "text" in part && typeof part.text === "string") {
        return part.text;
      }
      return "";
    })
    .join("");
}

/**
 * Messages that identify the turn, with the tool result being resumed stripped
 * off. The follow-up request then hashes to the same key as the request that
 * paused for those tool calls.
 */
export function keyMessages(messages: readonly KeyMessage[]): KeyMessage[] {
  let end = messages.length;
  while (end > 0 && messages[end - 1]?.role === "tool") end -= 1;
  const previous = messages[end - 1];
  if (previous?.role === "assistant" && (previous.tool_calls?.length ?? 0) > 0) {
    end -= 1;
  }
  return messages.slice(0, end);
}

function transcript(messages: readonly KeyMessage[]): string {
  return keyMessages(messages)
    .map((message) => {
      const calls = (message.tool_calls ?? [])
        .map((call) => `${call.id ?? ""}:${call.function?.name ?? ""}`)
        .join(",");
      return `${message.role}\n${message.tool_call_id ?? ""}\n${calls}\n${textOf(message.content)}`;
    })
    .join("\n---\n");
}

function digest(label: string, body: string): string {
  return createHash("sha256").update(`${label}:${body}`).digest("hex").slice(0, 16);
}

export function deriveConversationKey(messages: readonly KeyMessage[], sessionId = ""): string {
  return digest("conv", `${sessionId}\n${transcript(messages)}`);
}

export function deriveBridgeKey(
  modelId: string,
  messages: readonly KeyMessage[],
  sessionId = "",
): string {
  return digest("bridge", `${sessionId}\n${modelId}\n${transcript(messages)}`);
}
