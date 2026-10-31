import http from "node:http";
import { deriveBridgeKey, type KeyMessage } from "./session-key.ts";

function sse(data: unknown): string {
  return `data: ${JSON.stringify(data)}\n\ndata: [DONE]\n\n`;
}

function toolChunk(id: string, name: string, args: string): string {
  return sse({
    choices: [
      {
        delta: {
          tool_calls: [
            {
              index: 0,
              id,
              type: "function",
              function: { name, arguments: args },
            },
          ],
        },
      },
    ],
  });
}

function textChunk(text: string): string {
  return sse({ choices: [{ delta: { content: text } }] });
}

/**
 * OpenAI-shaped stand-in for the Cursor bridge. It never touches the workspace.
 * Tests use it to check that tool calls come back to the caller.
 */
export function startFixture(port = 0): Promise<{ port: number; close: () => Promise<void> }> {
  const pending = new Map<string, { id: string; name: string; args: string }>();
  const server = http.createServer(async (req, res) => {
    if (req.method === "GET" && req.url === "/health") {
      res.writeHead(200, { "content-type": "text/plain" });
      res.end("ok");
      return;
    }
    if (req.method === "GET" && req.url === "/v1/models") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          object: "list",
          data: [{ id: "composer-2.5", object: "model", owned_by: "cursor" }],
        }),
      );
      return;
    }
    if (req.method !== "POST" || req.url !== "/v1/chat/completions") {
      res.writeHead(404);
      res.end("Not Found");
      return;
    }

    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(Buffer.from(chunk));
    const body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>;
    delete body.thinking;
    delete body.reasoning_effort;
    const messages = (body.messages as KeyMessage[]) ?? [];
    const model = typeof body.model === "string" ? body.model : "";
    const session = String(req.headers["x-maki-session"] ?? "");
    const key = deriveBridgeKey(model, messages, session);
    const toolResults = messages.filter((message) => message.role === "tool");

    if (req.headers["x-fixture"] === "unknown") {
      res.writeHead(400, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          error: { message: "unknown tool", type: "invalid_request_error" },
        }),
      );
      return;
    }

    if (toolResults.length > 0 && pending.has(key)) {
      pending.delete(key);
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.end(textChunk("continued"));
      return;
    }

    const args = JSON.stringify({ path: "/tmp/maki-cursor-read.txt", offset: 1, limit: 0 });
    const id = `call_${key}`;
    pending.set(key, { id, name: "read", args });
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.end(toolChunk(id, "read", args));
  });

  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        reject(new Error("fixture failed to bind"));
        return;
      }
      resolve({
        port: address.port,
        close: () =>
          new Promise((done, fail) => {
            server.close((err) => (err ? fail(err) : done()));
          }),
      });
    });
  });
}
