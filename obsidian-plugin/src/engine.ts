/**
 * Three-way vault sync between a local `FileStore` and a `Remote`.
 *
 * The engine knows nothing about Obsidian or HTTP. It compares three things per path: the local
 * file, the server's file, and the state recorded the last time the two were in sync. Whichever
 * side moved away from that recorded state is the side that changed.
 */

import { errorMessage } from "./guards";

/** The server refuses larger files, so they are never read or sent. */
export const MAX_FILE_SIZE = 50 * 1024 * 1024;

const DEFAULT_CONCURRENCY = 4;
/** State is saved after this many changed entries, and always once more when a run ends. */
const PERSIST_EVERY = 20;
/** A run stops early after this many failures in a row (the server is most likely unreachable). */
const MAX_CONSECUTIVE_FAILURES = 5;
/** Local deletions above this count, when they are also most of the vault, need confirmation. */
const MASS_DELETION_MINIMUM = 10;

export type FileInfo = { path: string; size: number; mtime: number };
export type RemoteFile = FileInfo & { hash: string };
export type RemoteContent = { data: ArrayBuffer; hash: string };

export type WriteOutcome =
  | { kind: "written"; hash: string }
  | { kind: "conflict"; serverHash: string };

export type DeleteOutcome = { kind: "deleted" } | { kind: "conflict"; serverHash: string };

/** The device's files. Paths are relative and `/`-separated. */
export interface FileStore {
  list(): Promise<FileInfo[]>;
  /** Current size and mtime straight from storage, or null when the file does not exist. */
  stat(path: string): Promise<FileInfo | null>;
  read(path: string): Promise<ArrayBuffer>;
  /** Writes the file, creating parent folders as needed, and returns its new size and mtime. */
  write(path: string, data: ArrayBuffer): Promise<FileInfo>;
  remove(path: string): Promise<void>;
}

/** The server's sync API. `baseHash` is "" when the caller believes the file does not exist. */
export interface Remote {
  manifest(): Promise<RemoteFile[]>;
  /** Null when the server has no such file. */
  get(path: string): Promise<RemoteContent | null>;
  put(path: string, data: ArrayBuffer, baseHash: string): Promise<WriteOutcome>;
  delete(path: string, baseHash: string): Promise<DeleteOutcome>;
}

/** What one path looked like when it was last in sync: content hash plus the local size/mtime. */
export type SyncedEntry = { hash: string; mtime: number; size: number };
export type SyncState = Record<string, SyncedEntry>;

export type SyncConflict = { path: string; conflictPath: string };
export type SyncFailure = { path: string; message: string };

export type SyncSummary = {
  uploaded: number;
  downloaded: number;
  deletedLocal: number;
  deletedRemote: number;
  conflicts: SyncConflict[];
  errors: SyncFailure[];
  /** Paths left alone this run: over the size limit, or changed on disk while the run was going. */
  skipped: string[];
  /** False when the run stopped early after repeated failures. */
  completed: boolean;
};

export type SyncOptions = {
  local: FileStore;
  remote: Remote;
  /** Mutated in place as each file succeeds. */
  state: SyncState;
  /** Saves `state` durably. Called every few changes and once when the run ends. */
  persist: () => Promise<void>;
  deviceName: string;
  now?: () => Date;
  concurrency?: number;
  /** Set after the user confirms that most of the vault really was deleted on the server. */
  allowMassDeletion?: boolean;
};

/** The server lists far fewer files than before; deleting locally to match needs confirmation. */
export class MassDeletionError extends Error {
  readonly deletions: number;

  constructor(deletions: number, tracked: number) {
    super(
      `The server no longer has ${deletions} of ${tracked} synced files. ` +
        "Nothing was changed; confirm the deletion to continue.",
    );
    this.name = "MassDeletionError";
    this.deletions = deletions;
  }
}

/** True for relative paths the server accepts: no empty, `.`-prefixed or `..` component. */
export function isSyncablePath(path: string): boolean {
  if (path.length === 0 || path.includes("\\")) return false;
  return path.split("/").every((component) => component.length > 0 && !component.startsWith("."));
}

export async function sha256Hex(data: ArrayBuffer): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

/** `dir/Note.md` -> `dir/Note (conflict Phone 2026-01-31 235959).md`. */
export function conflictPathFor(path: string, deviceName: string, at: Date): string {
  const two = (value: number): string => String(value).padStart(2, "0");
  const stamp =
    `${at.getFullYear()}-${two(at.getMonth() + 1)}-${two(at.getDate())} ` +
    `${two(at.getHours())}${two(at.getMinutes())}${two(at.getSeconds())}`;
  // The device name becomes part of a file name, so it must not add folders or a leading dot.
  const device = deviceName.replace(/[\\/:*?"<>|.]/g, " ").replace(/\s+/g, " ").trim() || "device";
  const slash = path.lastIndexOf("/");
  const folder = path.slice(0, slash + 1);
  const name = path.slice(slash + 1);
  const dot = name.lastIndexOf(".");
  const stem = dot > 0 ? name.slice(0, dot) : name;
  const extension = dot > 0 ? name.slice(dot) : "";
  return `${folder}${stem} (conflict ${device} ${stamp})${extension}`;
}

type LocalFile = {
  info: FileInfo;
  hash: string;
  /** Present when the file had to be read to hash it; null when the stored hash was reused. */
  data: ArrayBuffer | null;
};

class SyncRun {
  readonly summary: SyncSummary = {
    uploaded: 0,
    downloaded: 0,
    deletedLocal: 0,
    deletedRemote: 0,
    conflicts: [],
    errors: [],
    skipped: [],
    completed: true,
  };

  private readonly local: FileStore;
  private readonly remote: Remote;
  private readonly state: SyncState;
  private readonly persist: () => Promise<void>;
  private readonly deviceName: string;
  private readonly now: () => Date;
  private readonly concurrency: number;
  private readonly allowMassDeletion: boolean;

  private localFiles = new Map<string, FileInfo>();
  private remoteFiles = new Map<string, RemoteFile>();
  /** Conflict copies created in this run, so two workers never pick the same name. */
  private readonly claimedPaths = new Set<string>();
  private unsavedChanges = 0;
  private saving: Promise<void> = Promise.resolve();

  constructor(options: SyncOptions) {
    this.local = options.local;
    this.remote = options.remote;
    this.state = options.state;
    this.persist = options.persist;
    this.deviceName = options.deviceName;
    this.now = options.now ?? (() => new Date());
    this.concurrency = Math.max(1, options.concurrency ?? DEFAULT_CONCURRENCY);
    this.allowMassDeletion = options.allowMassDeletion ?? false;
  }

  async run(): Promise<SyncSummary> {
    const [remoteList, localList] = await Promise.all([this.remote.manifest(), this.local.list()]);
    const oversized = new Set<string>();
    for (const file of localList) {
      if (!isSyncablePath(file.path)) continue;
      if (file.size > MAX_FILE_SIZE) oversized.add(file.path);
      else this.localFiles.set(file.path, file);
    }
    for (const file of remoteList) {
      if (!isSyncablePath(file.path)) continue;
      if (file.size > MAX_FILE_SIZE) oversized.add(file.path);
      else this.remoteFiles.set(file.path, file);
    }

    const paths = new Set<string>();
    for (const path of this.localFiles.keys()) paths.add(path);
    for (const path of this.remoteFiles.keys()) paths.add(path);
    for (const path of Object.keys(this.state)) if (isSyncablePath(path)) paths.add(path);
    // A path that is too large on either side is left completely alone: treating it as missing
    // on one side would turn into a deletion on the other.
    for (const path of oversized) paths.delete(path);
    this.summary.skipped.push(...[...oversized].sort());

    this.guardAgainstMassDeletion();

    try {
      await this.processAll([...paths].sort());
    } finally {
      await this.save();
    }
    return this.summary;
  }

  /**
   * A server pointed at an empty or wrong folder looks exactly like "everything was deleted".
   * Refuse to mirror that onto the device without confirmation.
   */
  private guardAgainstMassDeletion(): void {
    if (this.allowMassDeletion) return;
    const tracked = Object.keys(this.state).length;
    let deletions = 0;
    for (const path of Object.keys(this.state)) {
      if (this.localFiles.has(path) && !this.remoteFiles.has(path)) deletions += 1;
    }
    if (deletions > MASS_DELETION_MINIMUM && deletions * 2 > tracked) {
      throw new MassDeletionError(deletions, tracked);
    }
  }

  private async processAll(paths: string[]): Promise<void> {
    let next = 0;
    let consecutiveFailures = 0;
    const worker = async (): Promise<void> => {
      while (this.summary.completed) {
        const path = paths[next];
        next += 1;
        if (path === undefined) return;
        try {
          await this.syncPath(path);
          consecutiveFailures = 0;
        } catch (error) {
          // One file failing must not stop the others; it is reported and retried next run.
          this.summary.errors.push({ path, message: errorMessage(error) });
          consecutiveFailures += 1;
          if (consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) this.summary.completed = false;
        }
      }
    };
    await Promise.all(Array.from({ length: this.concurrency }, worker));
  }

  private async syncPath(path: string): Promise<void> {
    const base = this.state[path];
    const baseHash = base?.hash ?? null;
    const remoteHash = this.remoteFiles.get(path)?.hash ?? null;
    const local = await this.readLocal(path, base);
    const localHash = local?.hash ?? null;

    if (localHash === remoteHash) {
      if (local === null) this.forget(path);
      else this.record(path, local.hash, local.info);
      return;
    }
    if (localHash === baseHash) {
      await this.pull(path, local?.info ?? null);
      return;
    }
    if (remoteHash === baseHash) {
      if (local === null) await this.deleteRemote(path, baseHash ?? "");
      else await this.upload(path, local, baseHash ?? "");
      return;
    }
    // Both sides moved away from the last synced state, to different content.
    if (local === null) await this.pull(path, null);
    else if (remoteHash === null) await this.upload(path, local, "");
    else await this.keepBoth(path, local);
  }

  private async readLocal(path: string, base: SyncedEntry | undefined): Promise<LocalFile | null> {
    const info = this.localFiles.get(path);
    if (info === undefined) return null;
    // Same size and mtime as when it was last synced: trust the stored hash instead of re-reading.
    if (base !== undefined && base.mtime === info.mtime && base.size === info.size) {
      return { info, hash: base.hash, data: null };
    }
    const data = await this.local.read(path);
    return { info, hash: await sha256Hex(data), data };
  }

  /** Makes the local path match the server: download it, or delete it when the server has none. */
  private async pull(path: string, listed: FileInfo | null): Promise<void> {
    const content = await this.fetchVerified(path);
    if (!(await this.unchangedSinceListing(path, listed))) {
      this.summary.skipped.push(path);
      return;
    }
    if (content === null) {
      if (listed !== null) {
        await this.local.remove(path);
        this.summary.deletedLocal += 1;
      }
      this.forget(path);
      return;
    }
    const written = await this.local.write(path, content.data);
    this.record(path, content.hash, written);
    this.summary.downloaded += 1;
  }

  private async upload(path: string, local: LocalFile, baseHash: string): Promise<void> {
    const data = local.data ?? (await this.local.read(path));
    const outcome = await this.remote.put(path, data, baseHash);
    if (outcome.kind === "conflict") {
      await this.keepBoth(path, { ...local, data });
      return;
    }
    this.record(path, local.hash, local.info);
    this.summary.uploaded += 1;
  }

  private async deleteRemote(path: string, baseHash: string): Promise<void> {
    // The listing may be stale; only a file that is really gone may be deleted on the server.
    if ((await this.local.stat(path)) !== null) {
      this.summary.skipped.push(path);
      return;
    }
    const outcome = await this.remote.delete(path, baseHash);
    if (outcome.kind === "conflict") {
      // Someone changed the file after it was deleted here: their edit wins over the deletion.
      await this.pull(path, null);
      return;
    }
    this.forget(path);
    this.summary.deletedRemote += 1;
  }

  /**
   * Both sides changed. The server version takes the original path and the local version is
   * saved next to it and uploaded. Every network read happens before the first local write, and
   * the local copy is written before the original is replaced, so no step can lose either side.
   */
  private async keepBoth(path: string, local: LocalFile): Promise<void> {
    const localData = local.data ?? (await this.local.read(path));
    const content = await this.fetchVerified(path);
    if (content === null) {
      const outcome = await this.remote.put(path, localData, "");
      if (outcome.kind === "conflict") {
        throw new Error("The file keeps changing on the server; it will be retried next sync.");
      }
      this.record(path, local.hash, local.info);
      this.summary.uploaded += 1;
      return;
    }
    if (content.hash === local.hash) {
      this.record(path, local.hash, local.info);
      return;
    }
    if (!(await this.unchangedSinceListing(path, local.info))) {
      this.summary.skipped.push(path);
      return;
    }

    const conflictPath = await this.freeConflictPath(path);
    const copy = await this.local.write(conflictPath, localData);
    const original = await this.local.write(path, content.data);
    this.record(path, content.hash, original);
    this.summary.downloaded += 1;
    this.summary.conflicts.push({ path, conflictPath });

    // If this upload fails the copy is still on disk and goes up as a new file next run.
    const outcome = await this.remote.put(conflictPath, localData, "");
    if (outcome.kind === "conflict") {
      throw new Error(`The conflict copy "${conflictPath}" already exists on the server.`);
    }
    this.record(conflictPath, local.hash, copy);
    this.summary.uploaded += 1;
  }

  private async freeConflictPath(path: string): Promise<string> {
    const first = conflictPathFor(path, this.deviceName, this.now());
    const dot = first.lastIndexOf(".");
    const hasExtension = dot > first.lastIndexOf("/");
    let candidate = first;
    for (let attempt = 2; await this.isTaken(candidate); attempt += 1) {
      candidate = hasExtension
        ? `${first.slice(0, dot)} ${attempt}${first.slice(dot)}`
        : `${first} ${attempt}`;
    }
    this.claimedPaths.add(candidate);
    return candidate;
  }

  private async isTaken(path: string): Promise<boolean> {
    if (this.claimedPaths.has(path) || this.remoteFiles.has(path)) return true;
    return (await this.local.stat(path)) !== null;
  }

  /** Downloads a file and checks the bytes against the hash the server sent with them. */
  private async fetchVerified(path: string): Promise<RemoteContent | null> {
    const content = await this.remote.get(path);
    if (content === null) return null;
    const hash = await sha256Hex(content.data);
    if (content.hash !== "" && content.hash !== hash) {
      throw new Error("Downloaded content does not match the hash sent by the server.");
    }
    return { data: content.data, hash };
  }

  /**
   * Guards a local overwrite or delete: false when the file was edited, created or removed after
   * the run listed it. Such a path is skipped and picked up by the next run.
   */
  private async unchangedSinceListing(path: string, listed: FileInfo | null): Promise<boolean> {
    const current = await this.local.stat(path);
    if (listed === null || current === null) return listed === null && current === null;
    return current.mtime === listed.mtime && current.size === listed.size;
  }

  private record(path: string, hash: string, info: FileInfo): void {
    const previous = this.state[path];
    if (
      previous !== undefined &&
      previous.hash === hash &&
      previous.mtime === info.mtime &&
      previous.size === info.size
    ) {
      return;
    }
    this.state[path] = { hash, mtime: info.mtime, size: info.size };
    this.stateChanged();
  }

  private forget(path: string): void {
    if (this.state[path] === undefined) return;
    delete this.state[path];
    this.stateChanged();
  }

  private stateChanged(): void {
    this.unsavedChanges += 1;
    // A failed periodic save is not fatal here: the final save in `run` reports the failure.
    if (this.unsavedChanges >= PERSIST_EVERY) void this.save().catch(() => undefined);
  }

  private save(): Promise<void> {
    if (this.unsavedChanges === 0) return this.saving;
    this.unsavedChanges = 0;
    // Saves are chained so two never run at once and the last one always holds the final state.
    this.saving = this.saving.catch(() => undefined).then(() => this.persist());
    return this.saving;
  }
}

/**
 * Runs one sync pass. Safe to call again after a failure: state is recorded per file, and a file
 * whose state was not recorded is simply compared by content next time.
 */
export function syncVault(options: SyncOptions): Promise<SyncSummary> {
  return new SyncRun(options).run();
}
