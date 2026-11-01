import assert from "node:assert/strict";
import test from "node:test";
import { create, fromBinary, toBinary, toJson } from "@bufbuild/protobuf";
import {
  AgentClientMessageSchema,
  ExecServerMessageSchema,
  McpToolDefinitionSchema,
  ShellArgsSchema,
  ShellResultSchema,
} from "../src/proto/agent_pb.ts";
import { classifyExec } from "../src/exec-reply.ts";
import { concat, delimitedBody, lengthDelimited, stringField, varint } from "../src/proto-wire.ts";
import { answerPendingExecs, type AnsweredExec } from "../src/proxy.ts";

function unframe(frame: Uint8Array): Uint8Array {
  return frame.subarray(5);
}

function bashTool() {
  return create(McpToolDefinitionSchema, { name: "bash", toolName: "bash", description: "run" });
}

function execWith(field: number, payload: Uint8Array) {
  const exec = create(ExecServerMessageSchema, { id: 4, execId: "exec-4" });
  exec.$unknown = [{ no: field, wireType: 2, data: lengthDelimited(payload) }];
  return exec;
}

test("an mcp state request is answered with the connected tools and then closed", () => {
  const args = concat([
    stringField(1, "alpha"),
    stringField(1, "beta"),
    concat([varint((2 << 3) | 0), varint(1)]),
  ]);
  const disposition = classifyExec(execWith(36, args), [bashTool()]);
  assert.equal(disposition.kind, "immediate");
  if (disposition.kind !== "immediate") return;
  assert.equal(disposition.frames.length, 2);

  const state = fromBinary(AgentClientMessageSchema, disposition.frames[0]!);
  assert.equal(state.message.case, "execClientMessage");
  if (state.message.case !== "execClientMessage") return;
  const unknown = state.message.value.$unknown ?? [];
  assert.equal(unknown.length, 1);
  assert.equal(unknown[0]!.no, 36);
  const body = new TextDecoder().decode(delimitedBody(unknown[0]!.data));
  assert.match(body, /alpha/);
  assert.match(body, /beta/);
  assert.match(body, /connected/);
  assert.match(body, /bash/);

  const close = JSON.stringify(toJson(AgentClientMessageSchema, fromBinary(AgentClientMessageSchema, disposition.frames[1]!)));
  assert.match(close, /streamClose/);
  assert.match(close, /"id":4/);
});

test("a mini swe bash exec is run as a shell and answered on its own result field", () => {
  const args = toBinary(ShellArgsSchema, create(ShellArgsSchema, {
    command: "echo hi",
    workingDirectory: "/repo",
  }));
  const disposition = classifyExec(execWith(52, args), [bashTool()]);
  assert.equal(disposition.kind, "shell");
  if (disposition.kind !== "shell") return;
  assert.equal(disposition.variant.message.message.case, "shellArgs");
  assert.equal(disposition.variant.resultField, 55);

  const pending: AnsweredExec = {
    execId: "exec-4",
    execMsgId: 4,
    toolCallId: "call_bash",
    native: {
      resultType: "shellResult",
      args: { command: "echo hi", workingDirectory: "/repo" },
      resultField: disposition.variant.resultField,
    },
  };
  const frames: Uint8Array[] = [];
  answerPendingExecs([pending], [{ toolCallId: "call_bash", content: "hi\n" }], "", (frame) => frames.push(frame));
  assert.equal(frames.length, 2);
  const reply = fromBinary(AgentClientMessageSchema, unframe(frames[0]!));
  assert.equal(reply.message.case, "execClientMessage");
  if (reply.message.case !== "execClientMessage") return;
  const field = reply.message.value.$unknown?.find((item) => item.no === 55);
  assert.ok(field);
  const shell = fromBinary(ShellResultSchema, delimitedBody(field!.data));
  const json = JSON.stringify(toJson(ShellResultSchema, shell));
  assert.match(json, /echo hi/);
  assert.match(json, /hi/);
  assert.match(json, /success/);
  const close = JSON.stringify(toJson(AgentClientMessageSchema, fromBinary(AgentClientMessageSchema, unframe(frames[1]!))));
  assert.match(close, /streamClose/);
});

test("an unknown exec is refused and closed instead of left waiting", () => {
  const disposition = classifyExec(execWith(99, new Uint8Array([1])), [bashTool()]);
  assert.equal(disposition.kind, "immediate");
  if (disposition.kind !== "immediate") return;
  const joined = disposition.frames
    .map((frame) => JSON.stringify(toJson(AgentClientMessageSchema, fromBinary(AgentClientMessageSchema, frame))))
    .join("\n");
  assert.match(joined, /"throw"/);
  assert.match(joined, /99/);
  assert.match(joined, /streamClose/);
  assert.doesNotMatch(joined, /shellResult/);
});
