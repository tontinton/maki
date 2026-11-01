import assert from "node:assert/strict";
import http from "node:http";
import { mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fromBinary, toJson } from "@bufbuild/protobuf";
import { AgentClientMessageSchema } from "../src/proto/agent_pb.ts";
import {
  answerPendingExecs,
  saveConversation,
  startProxy,
  stopProxy,
  type AnsweredExec,
} from "../src/proxy.ts";

function unframe(frame: Uint8Array): Uint8Array {
  return frame.subarray(5);
}

function messageJson(frame: Uint8Array): string {
  const message = fromBinary(AgentClientMessageSchema, unframe(frame));
  return JSON.stringify(toJson(AgentClientMessageSchema, message));
}

function exec(kind: AnsweredExec["native"], id: string): AnsweredExec {
  return {
    execId: id,
    execMsgId: 7,
    toolCallId: `call_${id}`,
    native: kind,
  };
}

test("a denied write is a permission denial, not a successful write", () => {
  const frames: Uint8Array[] = [];
  answerPendingExecs(
    [exec({ resultType: "writeResult", args: { path: "/tmp/a.rs", fileSize: "4", linesCreated: "1" } }, "write")],
    [{ toolCallId: "call_write", content: "Permission denied for write" }],
    "",
    (frame) => frames.push(frame),
  );
  assert.equal(frames.length, 2);
  const write = messageJson(frames[0]);
  assert.match(write, /writeResult/);
  assert.match(write, /permissionDenied/);
  assert.match(write, /Permission denied for write/);
  assert.doesNotMatch(write, /"success"/);
  assert.match(messageJson(frames[1]), /streamClose/);
});

test("a failed shell reports its exit code and keeps the output", () => {
  const frames: Uint8Array[] = [];
  answerPendingExecs(
    [exec({ resultType: "shellResult", args: { command: "false", workingDirectory: "/tmp" } }, "shell")],
    [{ toolCallId: "call_shell", content: "boom\nExit code: 1" }],
    "",
    (frame) => frames.push(frame),
  );
  assert.equal(frames.length, 2);
  const shell = messageJson(frames[0]);
  assert.match(shell, /shellResult/);
  assert.match(shell, /failure/);
  assert.match(shell, /boom/);
  assert.match(shell, /"exitCode":1/);
  assert.doesNotMatch(shell, /"success"/);
  assert.match(messageJson(frames[1]), /streamClose/);
});

test("a finished shell returns its stdout as a typed shell result", () => {
  const frames: Uint8Array[] = [];
  answerPendingExecs(
    [exec({ resultType: "shellResult", args: { command: "ls", workingDirectory: "/repo" } }, "ls")],
    [{ toolCallId: "call_ls", content: "Cargo.toml\nsrc\n" }],
    "",
    (frame) => frames.push(frame),
  );
  assert.equal(frames.length, 2);
  const shell = messageJson(frames[0]);
  assert.match(shell, /shellResult/);
  assert.match(shell, /"success"/);
  assert.match(shell, /Cargo.toml/);
  assert.match(shell, /src/);
  assert.doesNotMatch(shell, /failure/);
  assert.doesNotMatch(shell, /permissionDenied/);
  assert.match(messageJson(frames[1]), /streamClose/);
});

test("a shell stream closes after the same exit code", () => {
  const frames: Uint8Array[] = [];
  answerPendingExecs(
    [exec({ resultType: "shellStreamResult", args: { command: "false", workingDirectory: "/tmp" } }, "stream")],
    [{ toolCallId: "call_stream", content: "boom\nExit code: 2" }],
    "",
    (frame) => frames.push(frame),
  );
  const joined = frames.map(messageJson).join("\n");
  assert.match(joined, /shellStream/);
  assert.match(joined, /boom/);
  assert.match(joined, /"code":2/);
  assert.equal(joined.match(/streamClose/g)?.length, 1);
  assert.doesNotMatch(joined, /"success"/);
});

test("conversation snapshots are readable only by this user", async () => {
  const dir = mkdtempSync(join(tmpdir(), "maki-cursor-conv-"));
  try {
    const file = await saveConversation("abcdef0123456789", {
      conversationId: "conv",
      checkpoint: null,
      blobStore: new Map(),
      lastAccessMs: Date.now(),
    }, dir);
    assert.ok(file);
    assert.equal(statSync(file).mode & 0o777, 0o600);
    assert.equal(statSync(dir).mode & 0o777, 0o700);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

function request(
  port: number,
  method: string,
  path: string,
  headers: Record<string, string>,
  body?: string,
): Promise<number> {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { hostname: "127.0.0.1", port, path, method, headers },
      (res) => {
        res.resume();
        res.on("end", () => resolve(res.statusCode ?? 0));
      },
    );
    req.on("error", reject);
    if (body) req.write(body);
    req.end();
  });
}

test("the proxy rejects callers that do not present its token or a loopback Host", async () => {
  const token = "test-token";
  const port = await startProxy(async () => "unused", [], 0, token);
  try {
    assert.equal(await request(port, "GET", "/health", {}), 401);
    assert.equal(
      await request(port, "GET", "/health", {
        host: "evil.example",
        authorization: `Bearer ${token}`,
      }),
      403,
    );
    assert.equal(
      await request(port, "GET", "/health", {
        host: `127.0.0.1:${port}`,
        authorization: `Bearer ${token}`,
      }),
      200,
    );
    assert.equal(
      await request(
        port,
        "POST",
        "/v1/chat/completions",
        {
          host: `127.0.0.1:${port}`,
          authorization: `Bearer ${token}`,
          "content-type": "text/plain",
        },
        "hello",
      ),
      415,
    );
  } finally {
    stopProxy();
  }
});
