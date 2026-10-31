/**
 * Native tool redirection.
 *
 * Cursor models aggressively call their built-in tools (read, shell, grep,
 * ls, write, fetch) before falling back to MCP tools. Rejecting those calls
 * burns model round-trips and confuses the model ("Tool not available in
 * this environment", issues #21/#29). When the client provides an equivalent
 * OpenAI tool, redirect the native call to it and convert the tool result
 * back into Cursor's typed native result frame.
 */
import { makiToolArgs, type NativeCall } from "./maki-args.ts";
import { create, toBinary } from "@bufbuild/protobuf";
import {
  AgentClientMessageSchema,
  ExecClientMessageSchema,
  FetchResultSchema,
  FetchSuccessSchema,
  GrepContentMatchSchema,
  GrepContentResultSchema,
  GrepCountResultSchema,
  GrepFileCountSchema,
  GrepFileMatchSchema,
  GrepFilesResultSchema,
  GrepResultSchema,
  GrepSuccessSchema,
  GrepUnionResultSchema,
  LsDirectoryTreeNodeSchema,
  LsDirectoryTreeNode_FileSchema,
  LsResultSchema,
  LsSuccessSchema,
  ReadResultSchema,
  ReadSuccessSchema,
  ShellResultSchema,
  ShellStreamExitSchema,
  ShellStreamSchema,
  ShellStreamStartSchema,
  ShellStreamStdoutSchema,
  ShellSuccessSchema,
  WriteResultSchema,
  WriteSuccessSchema,
  ExecClientControlMessageSchema,
  ExecClientStreamCloseSchema,
  type ExecServerMessage,
  type LsDirectoryTreeNode,
  type McpToolDefinition,
} from "./proto/agent_pb.ts";

export type NativeResultType =
  | "readResult"
  | "writeResult"
  | "fetchResult"
  | "shellResult"
  | "shellStreamResult"
  | "lsResult"
  | "grepResult";

/** How to answer the paused native exec once the redirected tool result arrives. */
export interface NativeExecBinding {
  resultType: NativeResultType;
  /** Native arg values needed to shape the typed result frame. */
  args: Record<string, string>;
}

export interface NativeRedirect {
  toolCallId: string;
  toolName: string;
  decodedArgs: string;
  binding: NativeExecBinding;
}

/**
 * Map a native exec request onto a client-provided OpenAI tool.
 * Returns null when no equivalent tool is available (caller rejects as before).
 */
function bindingFor(execCase: string, args: NativeCall["value"]): NativeExecBinding | null {
  if (execCase === "readArgs") {
    return { resultType: "readResult", args: { path: args.path ?? "" } };
  }
  if (execCase === "writeArgs") {
    const content =
      args.fileBytes && args.fileBytes.length > 0
        ? new TextDecoder().decode(args.fileBytes)
        : (args.fileText ?? "");
    return {
      resultType: "writeResult",
      args: {
        path: args.path ?? "",
        fileSize: String(new TextEncoder().encode(content).byteLength),
        linesCreated: String(content.split("\n").length),
      },
    };
  }
  if (execCase === "fetchArgs") {
    return { resultType: "fetchResult", args: { url: args.url ?? "" } };
  }
  if (execCase === "shellArgs" || execCase === "shellStreamArgs") {
    return {
      resultType: execCase === "shellStreamArgs" ? "shellStreamResult" : "shellResult",
      args: {
        command: args.command ?? "",
        workingDirectory: args.workingDirectory ?? "",
      },
    };
  }
  if (execCase === "lsArgs") {
    return { resultType: "lsResult", args: { path: args.path ?? "" } };
  }
  if (execCase === "grepArgs") {
    return {
      resultType: "grepResult",
      args: {
        pattern: args.pattern || args.glob || ".",
        path: args.path ?? "",
        outputMode: !args.pattern && args.glob ? "files_with_matches" : args.outputMode || "content",
        ...(args.multiline ? { multiline: "true" } : undefined),
        ...(args.headLimit != null ? { headLimit: String(args.headLimit) } : undefined),
      },
    };
  }
  return null;
}

/**
 * Map a native exec request onto a client-provided OpenAI tool.
 * Returns null when no equivalent tool is available (caller rejects as before).
 * Argument names come from `makiToolArgs`, so a redirected read is a maki `read`.
 */
export function redirectNativeExec(
  execMsg: ExecServerMessage,
  mcpTools: McpToolDefinition[],
): NativeRedirect | null {
  const execCase = execMsg.message.case;
  if (!execCase) return null;
  const args = execMsg.message.value as NativeCall["value"];
  const available = new Set(
    mcpTools.map((tool) => tool.name || tool.toolName).filter(Boolean),
  );
  const mapped = makiToolArgs(
    { case: execCase, value: args },
    available,
    args.toolCallId || crypto.randomUUID(),
  );
  const binding = bindingFor(execCase, args);
  if (!mapped || !binding) return null;
  return {
    toolCallId: mapped.toolCallId,
    toolName: mapped.toolName,
    decodedArgs: JSON.stringify(mapped.args),
    binding,
  };
}

interface PendingNativeExec {
  execId: string;
  execMsgId: number;
}

/**
 * Convert the redirected tool's text result into the typed native result the
 * paused exec expects. Returns false when no faithful conversion exists
 * (caller falls back to an mcpResult).
 * `sendMessage` receives an unframed AgentClientMessage binary.
 */
export function sendNativeExecResult(
  exec: PendingNativeExec,
  binding: NativeExecBinding,
  text: string,
  sendMessage: (bytes: Uint8Array) => void,
): boolean {
  const args = binding.args;

  const sendExec = (messageCase: string, value: unknown) => {
    const execClientMessage = create(ExecClientMessageSchema, {
      id: exec.execMsgId,
      execId: exec.execId,
      message: {
        case: messageCase as never,
        value: value as never,
      },
    });
    const clientMessage = create(AgentClientMessageSchema, {
      message: { case: "execClientMessage", value: execClientMessage },
    });
    sendMessage(toBinary(AgentClientMessageSchema, clientMessage));
  };

  switch (binding.resultType) {
    case "readResult": {
      sendExec(
        "readResult",
        create(ReadResultSchema, {
          result: {
            case: "success",
            value: create(ReadSuccessSchema, {
              path: args.path ?? "",
              totalLines: text.split("\n").length,
              fileSize: BigInt(new TextEncoder().encode(text).byteLength),
              truncated: false,
              output: { case: "content", value: text },
            }),
          },
        }),
      );
      return true;
    }

    case "writeResult": {
      sendExec(
        "writeResult",
        create(WriteResultSchema, {
          result: {
            case: "success",
            value: create(WriteSuccessSchema, {
              path: args.path ?? "",
              fileSize: Number(args.fileSize ?? 0),
              linesCreated: Number(args.linesCreated ?? 0),
            }),
          },
        }),
      );
      return true;
    }

    case "fetchResult": {
      sendExec(
        "fetchResult",
        create(FetchResultSchema, {
          result: {
            case: "success",
            value: create(FetchSuccessSchema, {
              url: args.url ?? "",
              content: text,
              statusCode: 200,
            }),
          },
        }),
      );
      return true;
    }

    case "shellResult": {
      sendExec(
        "shellResult",
        create(ShellResultSchema, {
          result: {
            case: "success",
            value: create(ShellSuccessSchema, {
              command: args.command ?? "",
              workingDirectory: args.workingDirectory ?? "",
              exitCode: 0,
              signal: "",
              stdout: text,
              stderr: "",
            }),
          },
        }),
      );
      return true;
    }

    case "shellStreamResult": {
      sendExec(
        "shellStream",
        create(ShellStreamSchema, {
          event: { case: "start", value: create(ShellStreamStartSchema, {}) },
        }),
      );
      if (text) {
        sendExec(
          "shellStream",
          create(ShellStreamSchema, {
            event: {
              case: "stdout",
              value: create(ShellStreamStdoutSchema, { data: text }),
            },
          }),
        );
      }
      sendExec(
        "shellStream",
        create(ShellStreamSchema, {
          event: { case: "exit", value: create(ShellStreamExitSchema, { code: 0 }) },
        }),
      );
      const controlMessage = create(ExecClientControlMessageSchema, {
        message: {
          case: "streamClose",
          value: create(ExecClientStreamCloseSchema, { id: exec.execMsgId }),
        },
      });
      const clientMessage = create(AgentClientMessageSchema, {
        message: { case: "execClientControlMessage", value: controlMessage },
      });
      sendMessage(toBinary(AgentClientMessageSchema, clientMessage));
      return true;
    }

    case "lsResult": {
      const built = buildLsResult(text, args.path ?? "");
      if (!built) return false;
      sendExec("lsResult", built);
      return true;
    }

    case "grepResult": {
      const built = buildGrepResult(text, args);
      if (!built) return false;
      sendExec("grepResult", built);
      return true;
    }
  }
}

/** Reconstruct Cursor's directory tree from glob output (one path per line). */
function buildLsResult(content: string, rootPath: string) {
  const normalizedRoot = rootPath || ".";
  const rawLines = content
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);

  const root = create(LsDirectoryTreeNodeSchema, {
    absPath: normalizedRoot,
    childrenDirs: [],
    childrenFiles: [],
    childrenWereProcessed: true,
    fullSubtreeExtensionCounts: {},
    numFiles: 0,
  });

  const dirMap = new Map<string, LsDirectoryTreeNode>([[normalizedRoot, root]]);

  for (const rawLine of rawLines) {
    const normalized = normalizeListedPath(rawLine, normalizedRoot);
    if (!normalized || normalized === normalizedRoot) continue;
    const relative =
      normalizedRoot !== "." && normalized.startsWith(`${normalizedRoot}/`)
        ? normalized.slice(normalizedRoot.length + 1)
        : normalized;
    const parts = relative.split("/").filter(Boolean);
    if (parts.length === 0) continue;

    let currentPath = normalizedRoot;
    let currentNode = dirMap.get(normalizedRoot)!;
    for (const segment of parts.slice(0, -1)) {
      const nextPath = joinPath(currentPath, segment);
      let nextNode = dirMap.get(nextPath);
      if (!nextNode) {
        nextNode = create(LsDirectoryTreeNodeSchema, {
          absPath: nextPath,
          childrenDirs: [],
          childrenFiles: [],
          childrenWereProcessed: true,
          fullSubtreeExtensionCounts: {},
          numFiles: 0,
        });
        currentNode.childrenDirs.push(nextNode);
        dirMap.set(nextPath, nextNode);
      }
      currentPath = nextPath;
      currentNode = nextNode;
    }

    const leaf = parts.at(-1)!;
    currentNode.childrenFiles.push(
      create(LsDirectoryTreeNode_FileSchema, { name: leaf }),
    );
  }

  computeLsStats(root);

  return create(LsResultSchema, {
    result: {
      case: "success",
      value: create(LsSuccessSchema, { directoryTreeRoot: root }),
    },
  });
}

function normalizeListedPath(path: string, rootPath: string): string {
  const cleaned = path.replace(/\/$/, "");
  if (!cleaned) return "";
  if (cleaned === ".") return rootPath || ".";
  if (cleaned.startsWith("/")) return cleaned;
  if (rootPath && rootPath !== ".") return joinPath(rootPath, cleaned);
  return cleaned;
}

function joinPath(base: string, segment: string): string {
  if (!base || base === ".") return segment;
  return `${base}/${segment}`;
}

function computeLsStats(node: LsDirectoryTreeNode): void {
  const extensionCounts: Record<string, number> = {};
  let numFiles = node.childrenFiles.length;

  for (const file of node.childrenFiles) {
    const dot = file.name.lastIndexOf(".");
    if (dot > 0 && dot < file.name.length - 1) {
      const ext = file.name.slice(dot + 1);
      extensionCounts[ext] = (extensionCounts[ext] ?? 0) + 1;
    }
  }

  for (const child of node.childrenDirs) {
    computeLsStats(child);
    numFiles += child.numFiles;
    for (const [ext, count] of Object.entries(child.fullSubtreeExtensionCounts)) {
      extensionCounts[ext] = (extensionCounts[ext] ?? 0) + count;
    }
  }

  node.numFiles = numFiles;
  node.fullSubtreeExtensionCounts = extensionCounts;
}

/** Parse the client grep tool's text output back into Cursor's structured result. */
function buildGrepResult(content: string, args: Record<string, string>) {
  const pattern = args.pattern ?? "";
  const path = args.path ?? "";
  const outputMode = args.outputMode || "content";

  if (args.multiline === "true") return null;
  if (!["content", "files_with_matches", "count"].includes(outputMode)) {
    return null;
  }

  const unionResult =
    outputMode === "count"
      ? buildGrepCountResult(content, Boolean(args.headLimit))
      : outputMode === "files_with_matches"
        ? buildGrepFilesResult(content, Boolean(args.headLimit))
        : buildGrepContentResult(content, Boolean(args.headLimit));

  // Non-empty tool output that we failed to parse: better to hand the raw
  // text back as an mcpResult than to claim "no matches".
  if (content.trim() && isEmptyGrepUnion(unionResult)) return null;

  return create(GrepResultSchema, {
    result: {
      case: "success",
      value: create(GrepSuccessSchema, {
        pattern,
        path,
        outputMode,
        workspaceResults: {
          [path || "."]: create(GrepUnionResultSchema, { result: unionResult }),
        },
      }),
    },
  });
}

function buildGrepCountResult(content: string, clientTruncated: boolean) {
  const counts: ReturnType<typeof create<typeof GrepFileCountSchema>>[] = [];
  let totalMatches = 0;
  for (const rawLine of content.split("\n")) {
    const line = rawLine.replace(/\r$/, "");
    if (!line) continue;
    const separator = line.lastIndexOf(":");
    if (separator === -1) continue;
    const tail = line.slice(separator + 1);
    if (!/^\d+$/.test(tail)) continue;
    const file = line.slice(0, separator);
    const count = Number.parseInt(tail, 10);
    counts.push(create(GrepFileCountSchema, { file, count }));
    totalMatches += count;
  }

  return {
    case: "count" as const,
    value: create(GrepCountResultSchema, {
      counts,
      totalFiles: counts.length,
      totalMatches,
      clientTruncated,
      ripgrepTruncated: false,
    }),
  };
}

function buildGrepFilesResult(content: string, clientTruncated: boolean) {
  const files = content
    .split("\n")
    .map((line) => line.replace(/\r$/, "").trim())
    .filter(Boolean);

  return {
    case: "files" as const,
    value: create(GrepFilesResultSchema, {
      files,
      totalFiles: files.length,
      clientTruncated,
      ripgrepTruncated: false,
    }),
  };
}

function buildGrepContentResult(content: string, clientTruncated: boolean) {
  const fileMatches: ReturnType<typeof create<typeof GrepFileMatchSchema>>[] = [];
  let currentFile = "";
  let currentMatches: ReturnType<typeof create<typeof GrepContentMatchSchema>>[] = [];
  let totalLines = 0;
  let totalMatchedLines = 0;

  const flushFile = () => {
    if (currentFile && currentMatches.length > 0) {
      fileMatches.push(
        create(GrepFileMatchSchema, { file: currentFile, matches: currentMatches }),
      );
    }
    currentMatches = [];
  };

  for (const rawLine of content.split("\n")) {
    const line = rawLine.replace(/\r$/, "");
    if (line === "--" || line === "") continue;

    const matchLine = line.match(/^(.+?):(\d+):(.*)/);
    if (matchLine) {
      const file = matchLine[1]!;
      if (file !== currentFile) {
        flushFile();
        currentFile = file;
      }
      totalLines += 1;
      totalMatchedLines += 1;
      currentMatches.push(
        create(GrepContentMatchSchema, {
          lineNumber: Number.parseInt(matchLine[2]!, 10),
          content: matchLine[3]!,
          contentTruncated: false,
          isContextLine: false,
        }),
      );
      continue;
    }

    const contextLine = parseGrepContextLine(line, currentFile);
    if (!contextLine) continue;
    if (contextLine.file !== currentFile) {
      flushFile();
      currentFile = contextLine.file;
    }
    totalLines += 1;
    currentMatches.push(
      create(GrepContentMatchSchema, {
        lineNumber: contextLine.lineNumber,
        content: contextLine.content,
        contentTruncated: false,
        isContextLine: true,
      }),
    );
  }

  flushFile();

  return {
    case: "content" as const,
    value: create(GrepContentResultSchema, {
      matches: fileMatches,
      totalLines,
      totalMatchedLines,
      clientTruncated,
      ripgrepTruncated: false,
    }),
  };
}

function parseGrepContextLine(line: string, currentFile: string) {
  if (currentFile) {
    const prefix = `${currentFile}-`;
    if (line.startsWith(prefix)) {
      const match = line.slice(prefix.length).match(/^(\d+)-(.*)$/s);
      if (match) {
        return {
          file: currentFile,
          lineNumber: Number.parseInt(match[1]!, 10),
          content: match[2]!,
        };
      }
    }
  }

  const fallback = line.match(/^(.+?)-(\d+)-(.*)$/);
  if (!fallback) return null;
  return {
    file: fallback[1]!,
    lineNumber: Number.parseInt(fallback[2]!, 10),
    content: fallback[3]!,
  };
}

function isEmptyGrepUnion(
  result:
    | ReturnType<typeof buildGrepCountResult>
    | ReturnType<typeof buildGrepFilesResult>
    | ReturnType<typeof buildGrepContentResult>,
): boolean {
  if (result.case === "count") return result.value.counts.length === 0;
  if (result.case === "files") return result.value.files.length === 0;
  return result.value.matches.length === 0;
}
