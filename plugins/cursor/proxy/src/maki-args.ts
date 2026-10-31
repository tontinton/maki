/** Cursor native exec fields the redirect reads. Plain data, so tests skip protobuf. */
export interface NativeCall {
  case: string;
  value: {
    toolCallId?: string;
    path?: string;
    command?: string;
    workingDirectory?: string;
    timeout?: number;
    url?: string;
    pattern?: string;
    glob?: string;
    fileText?: string;
    fileBytes?: Uint8Array;
    outputMode?: string;
    multiline?: boolean;
    headLimit?: number;
  };
}

export interface MakiToolCall {
  toolCallId: string;
  toolName: string;
  args: Record<string, unknown>;
}

function pick(available: ReadonlySet<string>, candidates: readonly string[]): string | undefined {
  return candidates.find((name) => available.has(name));
}

function textFrom(value: NativeCall["value"]): string {
  if (value.fileBytes && value.fileBytes.length > 0) {
    return new TextDecoder().decode(value.fileBytes);
  }
  return value.fileText ?? "";
}

/**
 * Map a Cursor built-in tool onto a maki tool call.
 * Returns null when maki did not offer an equivalent tool, so the caller
 * rejects the native call instead of running it here.
 */
export function makiToolArgs(
  call: NativeCall,
  available: ReadonlySet<string>,
  toolCallId: string,
): MakiToolCall | null {
  if (call.case === "readArgs") {
    const toolName = pick(available, ["read"]);
    if (!toolName) return null;
    return {
      toolCallId,
      toolName,
      args: { path: call.value.path ?? "", offset: 1, limit: 0 },
    };
  }

  if (call.case === "writeArgs") {
    const toolName = pick(available, ["write"]);
    if (!toolName) return null;
    return {
      toolCallId,
      toolName,
      args: { path: call.value.path ?? "", content: textFrom(call.value) },
    };
  }

  if (call.case === "fetchArgs") {
    const toolName = pick(available, ["webfetch", "fetch", "web_fetch"]);
    if (!toolName) return null;
    return {
      toolCallId,
      toolName,
      args: { url: call.value.url ?? "", format: "markdown" },
    };
  }

  if (call.case === "shellArgs" || call.case === "shellStreamArgs") {
    const toolName = pick(available, ["bash"]);
    if (!toolName) return null;
    const args: Record<string, unknown> = { command: call.value.command ?? "" };
    if (call.value.workingDirectory) args.workdir = call.value.workingDirectory;
    if ((call.value.timeout ?? 0) > 0) args.timeout = call.value.timeout;
    return { toolCallId, toolName, args };
  }

  if (call.case === "lsArgs") {
    const toolName = pick(available, ["glob"]);
    if (!toolName) return null;
    return {
      toolCallId,
      toolName,
      args: { pattern: "*", path: call.value.path ?? "" },
    };
  }

  if (call.case === "grepArgs") {
    if (!call.value.pattern && call.value.glob) {
      const toolName = pick(available, ["glob"]);
      if (!toolName) return null;
      return {
        toolCallId,
        toolName,
        args: { pattern: call.value.glob, path: call.value.path ?? "" },
      };
    }
    const toolName = pick(available, ["grep"]);
    if (!toolName) return null;
    const args: Record<string, unknown> = { pattern: call.value.pattern || "." };
    if (call.value.path) args.path = call.value.path;
    if (call.value.glob) args.include = call.value.glob;
    return { toolCallId, toolName, args };
  }

  return null;
}
