import assert from "node:assert/strict";
import test from "node:test";
import { asCompletionRequest } from "../src/proxy.ts";
import { redirectNativeExec } from "../src/native-tools.ts";

const tools = [
  { name: "read", toolName: "read" },
  { name: "write", toolName: "write" },
  { name: "bash", toolName: "bash" },
];

test("the live redirect emits maki read arguments", () => {
  const redirect = redirectNativeExec(
    {
      message: { case: "readArgs", value: { toolCallId: "c1", path: "/repo/src/main.rs" } },
    } as never,
    tools as never,
  );
  assert.ok(redirect);
  assert.equal(redirect.toolName, "read");
  assert.deepEqual(JSON.parse(redirect.decodedArgs), {
    path: "/repo/src/main.rs",
    offset: 1,
    limit: 0,
  });
});

test("the live redirect refuses deleteArgs", () => {
  const redirect = redirectNativeExec(
    { message: { case: "deleteArgs", value: { path: "/repo/src/main.rs" } } } as never,
    tools as never,
  );
  assert.equal(redirect, null);
});

test("completion requests drop thinking fields maki may add", () => {
  const request = asCompletionRequest({
    model: "composer-2.5",
    messages: [{ role: "user", content: "hi" }],
    thinking: { type: "disabled" },
    reasoning_effort: "high",
  });
  assert.equal("thinking" in request, false);
  assert.equal("reasoning_effort" in request, false);
  assert.equal(request.model, "composer-2.5");
});
