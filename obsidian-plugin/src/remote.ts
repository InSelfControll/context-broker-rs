/**
 * The Context Broker sync API over HTTP, with bearer-token handling.
 *
 * HTTP goes through an injected `fetch`-shaped function: the plugin passes a wrapper around
 * Obsidian's `requestUrl` (no CORS, works on Android), tests pass the real `fetch`.
 */

import type { DeleteOutcome, Remote, RemoteContent, RemoteFile, WriteOutcome } from "./engine";
import { isRecord, numberField, parseJsonRecord, stringField } from "./guards";

const REQUEST_TIMEOUT_MS = 120_000;

export type HttpRequestInit = {
  method: string;
  headers: Record<string, string>;
  body?: ArrayBuffer | string;
  signal?: AbortSignal;
};

export type HttpResponse = {
  status: number;
  headers: { get(name: string): string | null };
  arrayBuffer(): Promise<ArrayBuffer>;
};

/** The subset of `fetch` this plugin needs. */
export type HttpFetch = (url: string, init: HttpRequestInit) => Promise<HttpResponse>;

/** Where access tokens come from. Without sign-in (trusted network) `accessToken` returns null. */
export interface TokenSource {
  accessToken(): string | null;
  /** Gets a fresh access token with the refresh token; null when that is not possible. */
  refresh(): Promise<string | null>;
}

/** The server wants credentials that this device does not have (or that no longer work). */
export class AuthRequiredError extends Error {
  constructor() {
    super("The server requires sign-in.");
    this.name = "AuthRequiredError";
  }
}

export class HttpStatusError extends Error {
  readonly status: number;

  constructor(action: string, status: number) {
    super(`${action} failed with HTTP ${status}.`);
    this.name = "HttpStatusError";
    this.status = status;
  }
}

/** Strips a trailing slash and any path such as `/mcp`, leaving `https://host[:port]`. */
export function serverOrigin(serverUrl: string): string {
  return new URL(serverUrl.trim()).origin;
}

export class HttpRemote implements Remote {
  private readonly origin: string;
  private readonly http: HttpFetch;
  private readonly tokens: TokenSource;
  private refreshing: Promise<string | null> | null = null;

  constructor(serverUrl: string, http: HttpFetch, tokens: TokenSource) {
    this.origin = serverOrigin(serverUrl);
    this.http = http;
    this.tokens = tokens;
  }

  async manifest(): Promise<RemoteFile[]> {
    const response = await this.send("GET", "/sync/manifest", {});
    if (response.status !== 200) throw new HttpStatusError("Listing server files", response.status);
    const body = parseJsonRecord(await response.arrayBuffer());
    const files = body?.["files"];
    if (!Array.isArray(files)) {
      // Typically an HTML sign-in page from a proxy that answered instead of the broker.
      throw new Error("The server did not return a file list. Is sync enabled on the server?");
    }
    return files.map(parseRemoteFile);
  }

  async get(path: string): Promise<RemoteContent | null> {
    const response = await this.send("GET", fileUrl(path), {});
    if (response.status === 404) return null;
    if (response.status !== 200) throw new HttpStatusError(`Downloading "${path}"`, response.status);
    return {
      data: await response.arrayBuffer(),
      hash: response.headers.get("x-content-hash") ?? "",
    };
  }

  async put(path: string, data: ArrayBuffer, baseHash: string): Promise<WriteOutcome> {
    const headers = { "Content-Type": "application/octet-stream", "X-Base-Hash": baseHash };
    const response = await this.send("PUT", fileUrl(path), headers, data);
    if (response.status === 409) return { kind: "conflict", serverHash: await hashOf(response) };
    if (response.status !== 200) throw new HttpStatusError(`Uploading "${path}"`, response.status);
    return { kind: "written", hash: await hashOf(response) };
  }

  async delete(path: string, baseHash: string): Promise<DeleteOutcome> {
    const response = await this.send("DELETE", fileUrl(path), { "X-Base-Hash": baseHash });
    if (response.status === 409) return { kind: "conflict", serverHash: await hashOf(response) };
    // 404 means it is already gone, which is the state a delete asks for.
    if (response.status !== 200 && response.status !== 404) {
      throw new HttpStatusError(`Deleting "${path}" on the server`, response.status);
    }
    return { kind: "deleted" };
  }

  /** Sends a request with the bearer token; on 401 refreshes the token once and retries. */
  private async send(
    method: string,
    pathAndQuery: string,
    headers: Record<string, string>,
    body?: ArrayBuffer,
  ): Promise<HttpResponse> {
    const url = this.origin + pathAndQuery;
    const attempt = (token: string | null): Promise<HttpResponse> =>
      this.http(url, {
        method,
        headers: token === null ? headers : { ...headers, Authorization: `Bearer ${token}` },
        body,
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      });

    const token = this.tokens.accessToken();
    const response = await attempt(token);
    if (response.status !== 401) return response;

    // Another request may have refreshed already while this one was in flight.
    const current = this.tokens.accessToken();
    const fresh = current !== null && current !== token ? current : await this.refreshOnce();
    if (fresh === null) throw new AuthRequiredError();
    const retried = await attempt(fresh);
    if (retried.status === 401) throw new AuthRequiredError();
    return retried;
  }

  /** Requests run in parallel; refresh tokens rotate, so only one refresh may be in flight. */
  private refreshOnce(): Promise<string | null> {
    this.refreshing ??= this.tokens.refresh().finally(() => {
      this.refreshing = null;
    });
    return this.refreshing;
  }
}

function fileUrl(path: string): string {
  return `/sync/file?path=${encodeURIComponent(path)}`;
}

async function hashOf(response: HttpResponse): Promise<string> {
  const body = parseJsonRecord(await response.arrayBuffer());
  return body === null ? "" : (stringField(body, "hash") ?? "");
}

function parseRemoteFile(entry: unknown): RemoteFile {
  if (isRecord(entry)) {
    const path = stringField(entry, "path");
    const size = numberField(entry, "size");
    const mtime = numberField(entry, "mtime");
    const hash = stringField(entry, "hash");
    if (path !== null && size !== null && mtime !== null && hash !== null) {
      return { path, size, mtime, hash };
    }
  }
  throw new Error("The server returned a malformed file list entry.");
}
