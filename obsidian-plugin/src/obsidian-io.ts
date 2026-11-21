/** The engine's file and HTTP dependencies, implemented with Obsidian's API only (works on mobile). */

import { type App, normalizePath, requestUrl, TFile } from "obsidian";
import type { FileInfo, FileStore } from "./engine";
import type { HttpFetch } from "./remote";

export class VaultFileStore implements FileStore {
  private readonly app: App;

  constructor(app: App) {
    this.app = app;
  }

  /**
   * Uses the vault's in-memory index, so listing costs no disk access. The index can lag behind
   * the disk for a moment; the engine re-checks with `stat` before anything destructive.
   */
  list(): Promise<FileInfo[]> {
    const files = this.app.vault.getFiles();
    return Promise.resolve(
      files.map((file) => ({ path: file.path, size: file.stat.size, mtime: file.stat.mtime })),
    );
  }

  async stat(path: string): Promise<FileInfo | null> {
    const stat = await this.app.vault.adapter.stat(normalizePath(path));
    if (stat === null) return null;
    if (stat.type !== "file") throw new Error(`"${path}" is a folder on this device.`);
    return { path, size: stat.size, mtime: stat.mtime };
  }

  read(path: string): Promise<ArrayBuffer> {
    return this.app.vault.adapter.readBinary(normalizePath(path));
  }

  async write(path: string, data: ArrayBuffer): Promise<FileInfo> {
    const adapter = this.app.vault.adapter;
    const components = path.split("/");
    // Folders are created top-down, one after the other: each needs its parent to exist first.
    for (let depth = 1; depth < components.length; depth += 1) {
      const folder = normalizePath(components.slice(0, depth).join("/"));
      if (!(await adapter.exists(folder))) await adapter.mkdir(folder);
    }
    await adapter.writeBinary(normalizePath(path), data);
    const written = await this.stat(path);
    if (written === null) throw new Error(`"${path}" is missing right after it was written.`);
    return written;
  }

  /** Deletions go through the user's trash setting, so a file removed by sync can be restored. */
  async remove(path: string): Promise<void> {
    const file = this.app.vault.getAbstractFileByPath(normalizePath(path));
    if (file instanceof TFile) await this.app.fileManager.trashFile(file);
    else await this.app.vault.adapter.remove(normalizePath(path));
  }
}

/** `requestUrl` in the shape of `fetch`. It is not subject to CORS and works on Android. */
export const obsidianHttp: HttpFetch = async (url, init) => {
  const request = requestUrl({
    url,
    method: init.method,
    headers: init.headers,
    body: init.body,
    throw: false,
  });
  const response = await rejectOnAbort(request, init.signal);
  const headers = new Map<string, string>();
  for (const [name, value] of Object.entries(response.headers)) {
    headers.set(name.toLowerCase(), value);
  }
  return {
    status: response.status,
    headers: { get: (name) => headers.get(name.toLowerCase()) ?? null },
    arrayBuffer: () => Promise.resolve(response.arrayBuffer),
  };
};

/** `requestUrl` cannot be cancelled, so a timeout stops the wait rather than the request. */
function rejectOnAbort<T>(work: Promise<T>, signal: AbortSignal | undefined): Promise<T> {
  if (signal === undefined) return work;
  return new Promise<T>((resolve, reject) => {
    const onAbort = (): void => reject(new Error("The request timed out."));
    if (signal.aborted) onAbort();
    signal.addEventListener("abort", onAbort, { once: true });
    work.then(resolve, reject).finally(() => signal.removeEventListener("abort", onAbort));
  });
}
