/** Replies for exec messages this generated schema cannot name.
 *
 * Cursor's agent asks for MCP server state (field 36) before it will continue
 * a turn, and it waits until that exec stream closes. Dropping the message
 * leaves the turn silent until the client's read timeout. A shell variant this
 * schema also does not name (mini swe bash, field 52) is a normal shell and
 * must come back on its own result field (55), which is a ShellResult. */
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
  AgentClientMessageSchema,
  ExecClientControlMessageSchema,
  ExecClientMessageSchema,
  ExecClientStreamCloseSchema,
  ExecClientThrowSchema,
  ExecServerMessageSchema,
  McpToolDefinitionSchema,
  ShellArgsSchema,
  type ExecServerMessage,
  type McpToolDefinition,
} from "./proto/agent_pb.ts";
import {
  concat,
  delimitedBody,
  lengthDelimited,
  messageField,
  readMessage,
  stringField,
} from "./proto-wire.ts";

const MCP_STATE_ARGS = 36;
const MCP_STATE_RESULT = 36;
const MINI_SWE_BASH_ARGS = 52;
const MINI_SWE_BASH_RESULT = 55;
const LENGTH_DELIMITED = 2;

export interface UnknownExecField {
  no: number;
  data: Uint8Array;
}

export function unknownFields(message: { $unknown?: UnknownExecField[] }): UnknownExecField[] {
  return message.$unknown ?? [];
}

function agentMessage(value: ReturnType<typeof create<typeof ExecClientMessageSchema>> | ReturnType<typeof create<typeof ExecClientControlMessageSchema>>, kind: "execClientMessage" | "execClientControlMessage"): Uint8Array {
  const client = create(AgentClientMessageSchema, {
    message: { case: kind, value: value as never },
  });
  return toBinary(AgentClientMessageSchema, client);
}

export function streamCloseMessage(execMsgId: number): Uint8Array {
  return agentMessage(
    create(ExecClientControlMessageSchema, {
      message: {
        case: "streamClose",
        value: create(ExecClientStreamCloseSchema, { id: execMsgId }),
      },
    }),
    "execClientControlMessage",
  );
}

export function unavailableExec(execMsgId: number, error: string): Uint8Array[] {
  return [throwMessage(execMsgId, error), streamCloseMessage(execMsgId)];
}

function throwMessage(execMsgId: number, error: string): Uint8Array {
  return agentMessage(
    create(ExecClientControlMessageSchema, {
      message: {
        case: "throw",
        value: create(ExecClientThrowSchema, { id: execMsgId, error }),
      },
    }),
    "execClientControlMessage",
  );
}

function mcpStateSuccess(tools: readonly McpToolDefinition[], identifiers: readonly string[]): Uint8Array {
  const names = identifiers.length > 0 ? identifiers : ["maki"];
  const toolBytes = tools.map((tool) => toBinary(McpToolDefinitionSchema, tool));
  const servers = names.map((identifier) =>
    concat([
      stringField(1, identifier),
      stringField(2, identifier),
      ...toolBytes.map((tool) => messageField(5, tool)),
      stringField(7, "connected"),
    ]),
  );
  const success = concat(servers.map((server) => messageField(1, server)));
  return messageField(1, success);
}

function readStateArgs(data: Uint8Array): { identifiers: string[]; kickOnly: boolean } {
  const identifiers: string[] = [];
  let kickOnly = false;
  for (const field of readMessage(delimitedBody(data))) {
    if (field.field === 1 && field.wire === LENGTH_DELIMITED) {
      identifiers.push(new TextDecoder().decode(field.body));
    } else if (field.field === 2 && field.wire === 0 && field.body[0] === 1) {
      kickOnly = true;
    }
  }
  return { identifiers, kickOnly };
}

function execWithUnknownField(
  exec: ExecServerMessage,
  field: number,
  payload: Uint8Array,
): Uint8Array {
  const message = create(ExecClientMessageSchema, {
    id: exec.id,
    execId: exec.execId,
  });
  message.$unknown = [{ no: field, wireType: LENGTH_DELIMITED, data: lengthDelimited(payload) }];
  return agentMessage(message, "execClientMessage");
}

export interface ShellVariant {
  message: ExecServerMessage;
  /** ShellResult is returned on this field instead of shell_result. */
  resultField: number;
}

export type ExecDisposition =
  | { kind: "known" }
  | { kind: "shell"; variant: ShellVariant }
  | { kind: "immediate"; frames: Uint8Array[] };

/** Decide what an exec needs before the known handlers run. */
export function classifyExec(
  exec: ExecServerMessage,
  tools: readonly McpToolDefinition[],
): ExecDisposition {
  if (exec.message.case) return { kind: "known" };
  const fields = unknownFields(exec);
  const state = fields.find((field) => field.no === MCP_STATE_ARGS);
  if (state) {
    const args = readStateArgs(state.data);
    return {
      kind: "immediate",
      frames: [
        execWithUnknownField(exec, MCP_STATE_RESULT, mcpStateSuccess(tools, args.identifiers)),
        streamCloseMessage(exec.id),
      ],
    };
  }
  const bash = fields.find((field) => field.no === MINI_SWE_BASH_ARGS);
  if (bash) {
    const args = fromBinary(ShellArgsSchema, delimitedBody(bash.data));
    return {
      kind: "shell",
      variant: {
        message: create(ExecServerMessageSchema, {
          id: exec.id,
          execId: exec.execId,
          message: { case: "shellArgs", value: args },
        }),
        resultField: MINI_SWE_BASH_RESULT,
      },
    };
  }
  const name = fields.map((field) => field.no).join(",") || "none";
  return {
    kind: "immediate",
    frames: unavailableExec(
      exec.id,
      `Tool not available in this environment (${name}). Use the MCP tools provided instead.`,
    ),
  };
}
