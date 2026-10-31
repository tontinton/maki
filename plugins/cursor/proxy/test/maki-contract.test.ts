import assert from "node:assert/strict";
import { mkdtempSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chdir } from "node:process";
import test from "node:test";
import { makiToolArgs } from "../src/maki-args.ts";
import { deriveBridgeKey, deriveConversationKey } from "../src/session-key.ts";
import { startFixture } from "../src/fixture.ts";

const tools = new Set(["read", "write", "bash", "grep", "glob", "webfetch"]);

test("readArgs becomes a maki read with path, offset, and limit", () => {
  const call = makiToolArgs(
    { case: "readArgs", value: { toolCallId: "c1", path: "/tmp/a.rs" } },
    tools,
    "c1",
  );
  assert.ok(call);
  assert.equal(call.toolName, "read");
  assert.deepEqual(call.args, { path: "/tmp/a.rs", offset: 1, limit: 0 });
  assert.equal("filePath" in call.args, false);
});

test("writeArgs uses path and content", () => {
  const call = makiToolArgs(
    { case: "writeArgs", value: { path: "/tmp/a.rs", fileText: "fn main() {}\n" } },
    tools,
    "c2",
  );
  assert.ok(call);
  assert.equal(call.toolName, "write");
  assert.deepEqual(call.args, { path: "/tmp/a.rs", content: "fn main() {}\n" });
});

test("shellArgs uses bash command and workdir", () => {
  const call = makiToolArgs(
    {
      case: "shellArgs",
      value: { command: "git status", workingDirectory: "/repo", timeout: 5 },
    },
    tools,
    "c3",
  );
  assert.ok(call);
  assert.equal(call.toolName, "bash");
  assert.deepEqual(call.args, { command: "git status", workdir: "/repo", timeout: 5 });
});

test("an unknown native tool is not executed locally", () => {
  const call = makiToolArgs({ case: "deleteArgs", value: { path: "/tmp/a.rs" } }, tools, "c4");
  assert.equal(call, null);
});

test("a read without a maki read tool is refused", () => {
  const call = makiToolArgs(
    { case: "readArgs", value: { path: "/tmp/a.rs" } },
    new Set(["bash"]),
    "c5",
  );
  assert.equal(call, null);
});

test("resume keeps the bridge key and a different session does not", () => {
  const first = [
    { role: "system", content: "rules" },
    { role: "user", content: "read the parser" },
  ];
  const resumed = [
    ...first,
    {
      role: "assistant",
      tool_calls: [{ id: "call_1", function: { name: "read" } }],
    },
    { role: "tool", tool_call_id: "call_1", content: "fn main() {}" },
  ];
  const other = [
    { role: "system", content: "rules" },
    { role: "user", content: "read the tests" },
  ];
  assert.equal(deriveBridgeKey("composer-2.5", resumed, "sess-a"), deriveBridgeKey("composer-2.5", first, "sess-a"));
  assert.equal(deriveConversationKey(resumed, "sess-a"), deriveConversationKey(first, "sess-a"));
  assert.notEqual(deriveBridgeKey("composer-2.5", first, "sess-a"), deriveBridgeKey("composer-2.5", other, "sess-a"));
  assert.notEqual(deriveBridgeKey("composer-2.5", first, "sess-a"), deriveBridgeKey("composer-2.5", first, "sess-b"));
  assert.notEqual(deriveBridgeKey("composer-2.5", first, "sess-a"), deriveBridgeKey("auto", first, "sess-a"));
  assert.equal(deriveConversationKey(first, "sess-a"), deriveConversationKey(first, "sess-a"));
});

async function post(port: number, body: unknown, headers: Record<string, string> = {}): Promise<Response> {
  return fetch(`http://127.0.0.1:${port}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
}

test("fixture returns a maki read call, resumes it, and does not write the workspace", async () => {
  const dir = mkdtempSync(join(tmpdir(), "maki-cursor-"));
  const before = readdirSync(dir);
  const previous = process.cwd();
  chdir(dir);
  let fixture: Awaited<ReturnType<typeof startFixture>> | undefined;
  try {
    fixture = await startFixture(0);
    const first = await post(fixture.port, {
      model: "composer-2.5",
      thinking: { type: "disabled" },
      messages: [
        { role: "system", content: "rules" },
        { role: "user", content: "read the parser" },
      ],
      tools: [{ type: "function", function: { name: "read" } }],
    }, { "x-maki-session": "sess-a" });
    assert.equal(first.status, 200);
    const firstBody = await first.text();
    const payload = JSON.parse(firstBody.split("\n")[0].replace(/^data: /, ""));
    const toolCall = payload.choices[0].delta.tool_calls[0];
    assert.equal(toolCall.function.name, "read");
    assert.deepEqual(JSON.parse(toolCall.function.arguments), {
      path: "/tmp/maki-cursor-read.txt",
      offset: 1,
      limit: 0,
    });
    const id = toolCall.id;
    const second = await post(fixture.port, {
      model: "composer-2.5",
      messages: [
        { role: "system", content: "rules" },
        { role: "user", content: "read the parser" },
        { role: "assistant", tool_calls: [{ id, function: { name: "read" } }] },
        { role: "tool", tool_call_id: id, content: "fn main() {}" },
      ],
    }, { "x-maki-session": "sess-a" });
    assert.equal(second.status, 200);
    assert.match(await second.text(), /continued/);

    const unknown = await post(fixture.port, {
      model: "composer-2.5",
      messages: [{ role: "user", content: "delete it" }],
    }, { "x-fixture": "unknown", "x-maki-session": "sess-a" });
    assert.equal(unknown.status, 400);
    const unknownBody = await unknown.json() as { error: { message: string } };
    assert.equal(unknownBody.error.message, "unknown tool");
    assert.deepEqual(readdirSync(dir), before);
  } finally {
    chdir(previous);
    await fixture?.close();
  }
});
