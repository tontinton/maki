import { spawn } from "node:child_process";
import { access, chmod, readFile, rm, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import {
  generateCursorAuthParams,
  getTokenExpiry,
  pollCursorAuth,
  refreshCursorToken,
} from "./auth.ts";
import { startFixture } from "./fixture.ts";

const root = resolve(import.meta.dirname, "..");
const credentialsPath = process.env.MAKI_CURSOR_CREDENTIALS ?? resolve(root, "credentials.json");
const port = Number(process.env.MAKI_CURSOR_PORT ?? "53621");

interface StoredCredentials {
  access: string;
  refresh: string;
  expires: number;
}

async function readCredentials(): Promise<StoredCredentials | undefined> {
  try {
    const parsed = JSON.parse(await readFile(credentialsPath, "utf8")) as StoredCredentials;
    if (!parsed.access || !parsed.refresh) return undefined;
    return parsed;
  } catch {
    return undefined;
  }
}

async function writeCredentials(creds: StoredCredentials): Promise<void> {
  await writeFile(credentialsPath, JSON.stringify(creds), { mode: 0o600 });
  await chmod(credentialsPath, 0o600);
}

function npmCi(): Promise<void> {
  return new Promise((resolvePromise, reject) => {
    const child = spawn("npm", ["ci", "--omit=dev"], { cwd: root, stdio: "inherit" });
    child.on("error", reject);
    child.on("exit", (code) => {
      if (code === 0) resolvePromise();
      else reject(new Error(`npm ci exited ${code}`));
    });
  });
}

async function ensure(): Promise<void> {
  try {
    await access(resolve(root, "node_modules/@bufbuild/protobuf/package.json"));
  } catch {
    await npmCi();
  }
}

async function serve(): Promise<void> {
  await ensure();
  const { startProxy } = await import("./proxy.ts");
  const { getCursorModels } = await import("./models.ts");
  const current = await readCredentials();
  const models = await getCursorModels(current?.access ?? "");
  const bound = await startProxy(
    async () => {
      const creds = await readCredentials();
      if (!creds) throw new Error("run maki auth login cursor");
      if (creds.expires < Date.now()) {
        const refreshed = await refreshCursorToken(creds.refresh);
        await writeCredentials(refreshed);
        return refreshed.access;
      }
      return creds.access;
    },
    models,
    port,
  );
  console.log(`listening ${bound}`);
  await new Promise(() => {});
}

async function loginUrl(): Promise<void> {
  const statePath = process.argv[3];
  if (!statePath) throw new Error("login-url needs a state path");
  const params = await generateCursorAuthParams();
  await writeFile(statePath, JSON.stringify(params), { mode: 0o600 });
  console.log(params.loginUrl);
}

async function loginPoll(): Promise<void> {
  const statePath = process.argv[3];
  if (!statePath) throw new Error("login-poll needs a state path");
  const params = JSON.parse(await readFile(statePath, "utf8")) as {
    uuid: string;
    verifier: string;
  };
  const tokens = await pollCursorAuth(params.uuid, params.verifier);
  const creds = {
    access: tokens.accessToken,
    refresh: tokens.refreshToken,
    expires: getTokenExpiry(tokens.accessToken),
  };
  await writeCredentials(creds);
  await rm(statePath, { force: true });
  console.log(JSON.stringify(creds));
}

async function refresh(): Promise<void> {
  const creds = await readCredentials();
  if (!creds) throw new Error("run maki auth login cursor");
  const refreshed = await refreshCursorToken(creds.refresh);
  await writeCredentials(refreshed);
  console.log(JSON.stringify(refreshed));
}

async function status(): Promise<void> {
  const creds = await readCredentials();
  if (!creds) process.exit(1);
}

async function deps(): Promise<void> {
  try {
    await access(resolve(root, "node_modules/@bufbuild/protobuf/package.json"));
  } catch {
    process.exit(1);
  }
}

async function logout(): Promise<void> {
  await rm(credentialsPath, { force: true });
}

async function health(): Promise<void> {
  const response = await fetch(`http://127.0.0.1:${port}/health`);
  if (!response.ok) process.exit(1);
}

async function waitUntilReady(): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/health`);
      if (response.ok) return;
    } catch {
      // The server is still starting.
    }
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 100));
  }
  throw new Error("cursor proxy did not become ready");
}

async function fixture(): Promise<void> {
  const bound = await startFixture(port);
  console.log(`listening ${bound.port}`);
  await new Promise(() => {});
}

const command = process.argv[2];
try {
  if (command === "ensure") await ensure();
  else if (command === "serve") await serve();
  else if (command === "login-url") await loginUrl();
  else if (command === "login-poll") await loginPoll();
  else if (command === "refresh") await refresh();
  else if (command === "status") await status();
  else if (command === "deps") await deps();
  else if (command === "logout") await logout();
  else if (command === "health") await health();
  else if (command === "wait") await waitUntilReady();
  else if (command === "fixture") await fixture();
  else {
    throw new Error(`unknown command ${command ?? ""}`);
  }
} catch (err) {
  const message = err instanceof Error ? err.message : String(err);
  console.error(message);
  process.exit(1);
}
