/**
 * The one end-to-end test: two simulated devices sync real folders through the real broker
 * binary. Nothing on the server side is faked.
 */

import { afterAll, beforeAll, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readdir, readFile, rm, stat, truncate, unlink, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { parsePluginData } from "../src/data";
import {
  type FileInfo,
  type FileStore,
  MassDeletionError,
  MAX_FILE_SIZE,
  type Remote,
  type RemoteFile,
  type SyncState,
  type SyncSummary,
  syncVault,
} from "../src/engine";
import { type HttpFetch, HttpRemote, type TokenSource } from "../src/remote";

const BROKER_BINARY = resolve(import.meta.dir, "../../target/release/context-broker");
const CONFLICT_TIME = new Date(2026, 0, 31, 23, 59, 58);
const CONFLICT_STAMP = "2026-01-31 235958";

/** A `FileStore` over a real folder. Counts reads so the test can prove hashes are reused. */
class FolderStore implements FileStore {
  reads = 0;

  constructor(readonly root: string) {}

  async list(): Promise<FileInfo[]> {
    const entries = await readdir(this.root, { recursive: true, withFileTypes: true });
    const files = entries.filter((entry) => entry.isFile());
    return Promise.all(
      files.map(async (entry) => {
        const absolute = join(entry.parentPath, entry.name);
        const path = absolute.slice(this.root.length + 1).split("\\").join("/");
        const info = await stat(absolute);
        return { path, size: info.size, mtime: info.mtimeMs };
      }),
    );
  }

  async stat(path: string): Promise<FileInfo | null> {
    try {
      const info = await stat(join(this.root, path));
      return { path, size: info.size, mtime: info.mtimeMs };
    } catch (error) {
      if (isMissingFile(error)) return null;
      throw error;
    }
  }

  async read(path: string): Promise<ArrayBuffer> {
    this.reads += 1;
    return toArrayBuffer(await readFile(join(this.root, path)));
  }

  async write(path: string, data: ArrayBuffer): Promise<FileInfo> {
    const absolute = join(this.root, path);
    await mkdir(dirname(absolute), { recursive: true });
    await writeFile(absolute, new Uint8Array(data));
    const info = await stat(absolute);
    return { path, size: info.size, mtime: info.mtimeMs };
  }

  async remove(path: string): Promise<void> {
    await unlink(join(this.root, path));
  }
}

function isMissingFile(error: unknown): boolean {
  return error instanceof Error && "code" in error && error.code === "ENOENT";
}

function toArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  const copy = new ArrayBuffer(bytes.byteLength);
  new Uint8Array(copy).set(bytes);
  return copy;
}

/** Counts calls, and optionally serves an old manifest or fails after a number of calls. */
class ObservedRemote implements Remote {
  calls = 0;
  conflictResponses = 0;
  staleManifest: RemoteFile[] | null = null;
  failAfterCalls = Number.POSITIVE_INFINITY;

  constructor(private readonly inner: Remote) {}

  private count(): void {
    this.calls += 1;
    if (this.calls > this.failAfterCalls) throw new Error("simulated network failure");
  }

  async manifest(): Promise<RemoteFile[]> {
    this.count();
    return this.staleManifest ?? (await this.inner.manifest());
  }

  async get(path: string) {
    this.count();
    return this.inner.get(path);
  }

  async put(path: string, data: ArrayBuffer, baseHash: string) {
    this.count();
    const outcome = await this.inner.put(path, data, baseHash);
    if (outcome.kind === "conflict") this.conflictResponses += 1;
    return outcome;
  }

  async delete(path: string, baseHash: string) {
    this.count();
    const outcome = await this.inner.delete(path, baseHash);
    if (outcome.kind === "conflict") this.conflictResponses += 1;
    return outcome;
  }
}

type SyncRunResult = { summary: SyncSummary; remote: ObservedRemote; reads: number };

/** One simulated device: a vault folder plus a state file, reloaded from disk on every sync. */
class Device {
  readonly store: FolderStore;

  constructor(
    readonly name: string,
    readonly vault: string,
    readonly stateFile: string,
    private readonly serverUrl: string,
  ) {
    this.store = new FolderStore(vault);
  }

  async sync(
    configure: (remote: ObservedRemote) => void = () => undefined,
    options: { allowMassDeletion?: boolean; concurrency?: number } = {},
  ): Promise<SyncRunResult> {
    const http: HttpFetch = fetch;
    const noTokens: TokenSource = { accessToken: () => null, refresh: () => Promise.resolve(null) };
    const remote = new ObservedRemote(new HttpRemote(this.serverUrl, http, noTokens));
    configure(remote);
    const state = await this.loadState();
    const readsBefore = this.store.reads;
    const summary = await syncVault({
      local: this.store,
      remote,
      state,
      persist: () => writeFile(this.stateFile, JSON.stringify(state)),
      deviceName: this.name,
      now: () => CONFLICT_TIME,
      ...options,
    });
    return { summary, remote, reads: this.store.reads - readsBefore };
  }

  private async loadState(): Promise<SyncState> {
    try {
      // Parsed with the plugin's own loader, as it would be after an app restart.
      const saved: unknown = JSON.parse(await readFile(this.stateFile, "utf8"));
      return parsePluginData({ syncState: saved }, this.name).syncState;
    } catch (error) {
      if (isMissingFile(error)) return {};
      throw error;
    }
  }

  async put(path: string, content: string | Uint8Array): Promise<void> {
    const absolute = join(this.vault, path);
    await mkdir(dirname(absolute), { recursive: true });
    await writeFile(absolute, content);
  }

  text(path: string): Promise<string> {
    return readFile(join(this.vault, path), "utf8");
  }

  async remove(path: string): Promise<void> {
    await unlink(join(this.vault, path));
  }
}

/** Path -> sha256 for every file outside dot-folders: what should be identical everywhere. */
async function snapshot(root: string, except: string[] = []): Promise<Record<string, string>> {
  const entries = await readdir(root, { recursive: true, withFileTypes: true });
  const result: Record<string, string> = {};
  for (const entry of entries) {
    if (!entry.isFile()) continue;
    const absolute = join(entry.parentPath, entry.name);
    const path = absolute.slice(root.length + 1);
    if (path.split("/").some((component) => component.startsWith(".")) || except.includes(path)) {
      continue;
    }
    result[path] = createHash("sha256").update(await readFile(absolute)).digest("hex");
  }
  return Object.fromEntries(Object.entries(result).sort(([a], [b]) => a.localeCompare(b)));
}

function counts(summary: SyncSummary) {
  return {
    uploaded: summary.uploaded,
    downloaded: summary.downloaded,
    deletedLocal: summary.deletedLocal,
    deletedRemote: summary.deletedRemote,
    conflicts: summary.conflicts.length,
    errors: summary.errors,
  };
}

function expectCounts(summary: SyncSummary, expected: Partial<ReturnType<typeof counts>>): void {
  expect(counts(summary)).toEqual({
    uploaded: 0,
    downloaded: 0,
    deletedLocal: 0,
    deletedRemote: 0,
    conflicts: 0,
    errors: [],
    ...expected,
  });
}

function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const probe = createServer();
    probe.on("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const address = probe.address();
      probe.close(() => {
        if (address !== null && typeof address === "object") resolvePort(address.port);
        else reject(new Error("could not find a free port"));
      });
    });
  });
}

let workspace = "";
let serverVault = "";
let serverUrl = "";
let server: ReturnType<typeof Bun.spawn> | null = null;

beforeAll(async () => {
  workspace = await mkdtemp(join(tmpdir(), "context-broker-sync-e2e-"));
  serverVault = join(workspace, "server-vault");
  const storage = join(workspace, "server-storage");
  const cwd = join(workspace, "server-cwd");
  await Promise.all([serverVault, storage, cwd].map((folder) => mkdir(folder)));

  const port = await freePort();
  serverUrl = `http://127.0.0.1:${port}`;
  server = Bun.spawn([BROKER_BINARY], {
    cwd,
    env: {
      ...process.env,
      CONTEXT_BROKER_HTTP_ADDR: `127.0.0.1:${port}`,
      CONTEXT_BROKER_PROJECT_ROOT: serverVault,
      CONTEXT_BROKER_SYNC: "1",
      CONTEXT_BROKER_STORAGE_DIR: storage,
    },
    stdin: "ignore",
    stdout: Bun.file(join(workspace, "server.stdout.log")),
    stderr: Bun.file(join(workspace, "server.stderr.log")),
  });

  const deadline = Date.now() + 30_000;
  for (;;) {
    try {
      const response = await fetch(`${serverUrl}/sync/manifest`, {
        signal: AbortSignal.timeout(2000),
      });
      if (response.status === 200) return;
      throw new Error(`GET /sync/manifest returned HTTP ${response.status} (sync API missing?)`);
    } catch (error) {
      const refused = error instanceof Error && "code" in error && error.code === "ECONNREFUSED";
      if (!refused || Date.now() > deadline) throw error;
      await Bun.sleep(100);
    }
  }
});

afterAll(async () => {
  if (server !== null) {
    server.kill();
    await server.exited;
  }
  if (workspace !== "") await rm(workspace, { recursive: true, force: true });
});

test(
  "two devices stay in sync through the real broker",
  async () => {
    const deviceA = new Device("Laptop A", join(workspace, "a"), join(workspace, "a.json"), serverUrl);
    const deviceB = new Device("Phone B", join(workspace, "b"), join(workspace, "b.json"), serverUrl);
    await Promise.all([mkdir(deviceA.vault), mkdir(deviceB.vault)]);
    // The oversized file only ever exists on A, so it is left out of A's side of comparisons.
    const expectConverged = async (): Promise<Record<string, string>> => {
      const [a, b, onServer] = await Promise.all([
        snapshot(deviceA.vault, ["big.bin"]),
        snapshot(deviceB.vault),
        snapshot(serverVault),
      ]);
      expect(a).toEqual(onServer);
      expect(b).toEqual(onServer);
      return onServer;
    };

    // --- Initial upload from A: text, nested folders, odd names, binary, empty, excluded paths.
    const binary = new Uint8Array(1024).map((_, index) => index % 256);
    const oddName = "deep/nested/dir/file with spaces & ünïcode #1 100%.md";
    await deviceA.put("Welcome.md", "# Welcome\n");
    await deviceA.put("notes/a.md", "alpha v1\n");
    await deviceA.put("notes/b.md", "beta v1\n");
    await deviceA.put("notes/c.md", "gamma v1\n");
    await deviceA.put("img/pixel.bin", binary);
    await deviceA.put(oddName, "odd name\n");
    await deviceA.put("empty.md", "");
    await deviceA.put(".obsidian/x.json", '{"device":"A"}');
    await deviceA.put("big.bin", "");
    await truncate(join(deviceA.vault, "big.bin"), MAX_FILE_SIZE + 1);
    await deviceB.put(".obsidian/x.json", '{"device":"B"}');

    let run = await deviceA.sync();
    expectCounts(run.summary, { uploaded: 7 });
    expect(run.summary.skipped).toEqual(["big.bin"]);
    expect(run.summary.completed).toBe(true);
    expect(Object.keys(await snapshot(serverVault))).toEqual(
      ["Welcome.md", oddName, "empty.md", "img/pixel.bin", "notes/a.md", "notes/b.md", "notes/c.md"].sort(
        (a, b) => a.localeCompare(b),
      ),
    );

    // --- B receives everything, byte for byte.
    run = await deviceB.sync();
    expectCounts(run.summary, { downloaded: 7 });
    await expectConverged();
    expect(new Uint8Array(await readFile(join(deviceB.vault, "img/pixel.bin")))).toEqual(binary);
    expect(await deviceB.text(oddName)).toBe("odd name\n");
    expect(await deviceB.text("empty.md")).toBe("");

    // --- `.obsidian/` never leaves a device and is never overwritten.
    expect(await deviceB.text(".obsidian/x.json")).toBe('{"device":"B"}');
    expect(await deviceA.text(".obsidian/x.json")).toBe('{"device":"A"}');
    expect(await readdir(serverVault)).not.toContain(".obsidian");
    const manifest = await (await fetch(`${serverUrl}/sync/manifest`)).json();
    expect(JSON.stringify(manifest)).not.toContain(".obsidian");
    expect(JSON.stringify(manifest)).not.toContain("big.bin");

    // --- A second sync right after is a no-op: one manifest request, no file read or hashed.
    for (const device of [deviceA, deviceB]) {
      run = await device.sync();
      expectCounts(run.summary, {});
      expect(run.remote.calls).toBe(1);
      expect(run.reads).toBe(0);
    }

    // --- Edits each way.
    await deviceA.put("notes/a.md", "alpha v2, edited on A\n");
    expectCounts((await deviceA.sync()).summary, { uploaded: 1 });
    expectCounts((await deviceB.sync()).summary, { downloaded: 1 });
    expect(await deviceB.text("notes/a.md")).toBe("alpha v2, edited on A\n");
    await deviceB.put("Welcome.md", "# Welcome\nedited on B\n");
    expectCounts((await deviceB.sync()).summary, { uploaded: 1 });
    expectCounts((await deviceA.sync()).summary, { downloaded: 1 });
    expect(await deviceA.text("Welcome.md")).toBe("# Welcome\nedited on B\n");

    // --- Deletes each way.
    await deviceA.remove("notes/b.md");
    expectCounts((await deviceA.sync()).summary, { deletedRemote: 1 });
    expectCounts((await deviceB.sync()).summary, { deletedLocal: 1 });
    await deviceB.remove("notes/c.md");
    expectCounts((await deviceB.sync()).summary, { deletedRemote: 1 });
    expectCounts((await deviceA.sync()).summary, { deletedLocal: 1 });
    let converged = await expectConverged();
    expect(converged["notes/b.md"]).toBeUndefined();
    expect(converged["notes/c.md"]).toBeUndefined();

    // --- A new nested folder and a changed binary file, from B to A.
    const changedBinary = binary.slice().reverse();
    await deviceB.put("projects/2026/q4/plan.md", "plan v1\n");
    await deviceB.put("img/pixel.bin", changedBinary);
    expectCounts((await deviceB.sync()).summary, { uploaded: 2 });
    expectCounts((await deviceA.sync()).summary, { downloaded: 2 });
    expect(await deviceA.text("projects/2026/q4/plan.md")).toBe("plan v1\n");
    expect(new Uint8Array(await readFile(join(deviceA.vault, "img/pixel.bin")))).toEqual(changedBinary);

    // --- Simultaneous different edits: the server version keeps the path, B's becomes a copy.
    await deviceA.put("notes/a.md", "edited at the same time on A\n");
    await deviceB.put("notes/a.md", "edited at the same time on B\n");
    expectCounts((await deviceA.sync()).summary, { uploaded: 1 });
    run = await deviceB.sync();
    const noteCopy = `notes/a (conflict Phone B ${CONFLICT_STAMP}).md`;
    expectCounts(run.summary, { uploaded: 1, downloaded: 1, conflicts: 1 });
    expect(run.summary.conflicts).toEqual([{ path: "notes/a.md", conflictPath: noteCopy }]);
    expectCounts((await deviceA.sync()).summary, { downloaded: 1 });
    for (const device of [deviceA, deviceB]) {
      expect(await device.text("notes/a.md")).toBe("edited at the same time on A\n");
      expect(await device.text(noteCopy)).toBe("edited at the same time on B\n");
    }
    expect(await readFile(join(serverVault, "notes/a.md"), "utf8")).toBe("edited at the same time on A\n");
    expect(await readFile(join(serverVault, noteCopy), "utf8")).toBe("edited at the same time on B\n");
    await expectConverged();

    // --- The same race found late: B's manifest is already old when it uploads and deletes, so
    // the server itself answers 409 to the PUT and to the DELETE.
    const before = await new HttpRemote(serverUrl, fetch, {
      accessToken: () => null,
      refresh: () => Promise.resolve(null),
    }).manifest();
    await deviceA.put("Welcome.md", "welcome from A\n");
    await deviceA.put("projects/2026/q4/plan.md", "plan v2 from A\n");
    expectCounts((await deviceA.sync()).summary, { uploaded: 2 });
    await deviceB.put("Welcome.md", "welcome from B\n");
    await deviceB.remove("projects/2026/q4/plan.md");
    run = await deviceB.sync((remote) => {
      remote.staleManifest = before;
    });
    const welcomeCopy = `Welcome (conflict Phone B ${CONFLICT_STAMP}).md`;
    expect(run.remote.conflictResponses).toBe(2);
    expectCounts(run.summary, { uploaded: 1, downloaded: 2, conflicts: 1 });
    expect(run.summary.conflicts).toEqual([{ path: "Welcome.md", conflictPath: welcomeCopy }]);
    expect(await deviceB.text("Welcome.md")).toBe("welcome from A\n");
    expect(await deviceB.text(welcomeCopy)).toBe("welcome from B\n");
    expect(await deviceB.text("projects/2026/q4/plan.md")).toBe("plan v2 from A\n");
    expectCounts((await deviceA.sync()).summary, { downloaded: 1 });
    await expectConverged();

    // --- A file written straight into the server's folder reaches both devices.
    await mkdir(join(serverVault, "inbox"));
    await writeFile(join(serverVault, "inbox/from-server.md"), "written on the server\n");
    expectCounts((await deviceA.sync()).summary, { downloaded: 1 });
    expectCounts((await deviceB.sync()).summary, { downloaded: 1 });
    expect(await deviceB.text("inbox/from-server.md")).toBe("written on the server\n");

    // --- A sync cut off midway: three uploads get through, then the network dies.
    const bulk = Array.from({ length: 12 }, (_, index) => `bulk/n-${String(index).padStart(2, "0")}.md`);
    await Promise.all(bulk.map((path) => deviceA.put(path, `bulk ${path}\n`)));
    await deviceA.put("Welcome.md", "welcome, edited before the outage\n");
    // One request at a time here, so exactly which requests fail is the same on every run.
    run = await deviceA.sync(
      (remote) => {
        remote.failAfterCalls = 4;
      },
      { concurrency: 1 },
    );
    expect(run.summary.uploaded).toBe(3);
    expect(run.summary.completed).toBe(false);
    expect(run.summary.errors.length).toBe(5);
    expect(run.summary.errors[0]?.message).toBe("simulated network failure");
    const [partial, wanted] = await Promise.all([snapshot(serverVault), snapshot(deviceA.vault)]);
    const arrived = [...bulk, "Welcome.md"].filter((path) => partial[path] === wanted[path]);
    expect(arrived.length).toBe(3);

    // A clean run finishes exactly the rest and nothing is sent twice.
    expectCounts((await deviceA.sync()).summary, { uploaded: 10 });
    expectCounts((await deviceB.sync()).summary, { downloaded: 13 });
    converged = await expectConverged();
    expect(bulk.every((path) => converged[path] !== undefined)).toBe(true);

    // Losing the state file entirely (a crash before it was saved) is also harmless: files are
    // matched by content and nothing is transferred or duplicated.
    await unlink(deviceA.stateFile);
    run = await deviceA.sync();
    expectCounts(run.summary, {});
    expect(run.reads).toBeGreaterThan(0);

    // --- Everything is settled: one more sync on each device does nothing.
    for (const device of [deviceA, deviceB]) {
      run = await device.sync();
      expectCounts(run.summary, {});
      expect(run.remote.calls).toBe(1);
      expect(run.reads).toBe(0);
    }
    expect(await deviceA.text(".obsidian/x.json")).toBe('{"device":"A"}');
    expect(await deviceB.text(".obsidian/x.json")).toBe('{"device":"B"}');
    expect((await stat(join(deviceA.vault, "big.bin"))).size).toBe(MAX_FILE_SIZE + 1);

    // --- A server that suddenly lists nothing must not empty the device without confirmation.
    const tracked = Object.keys(converged);
    await Promise.all(tracked.map((path) => unlink(join(serverVault, path))));
    const beforeWipe = await snapshot(deviceA.vault);
    await expect(deviceA.sync()).rejects.toBeInstanceOf(MassDeletionError);
    expect(await snapshot(deviceA.vault)).toEqual(beforeWipe);
    run = await deviceA.sync(() => undefined, { allowMassDeletion: true });
    expectCounts(run.summary, { deletedLocal: tracked.length });
    expect(Object.keys(await snapshot(deviceA.vault))).toEqual(["big.bin"]);
  },
  120_000,
);
