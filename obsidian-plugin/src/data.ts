/** Everything the plugin persists with `saveData`: settings, sign-in and the sync state map. */

import type { SyncState } from "./engine";
import { booleanField, isRecord, type JsonRecord, numberField, stringField } from "./guards";
import type { OAuthEndpoints, TokenSet } from "./oauth";

export type Settings = {
  serverUrl: string;
  deviceName: string;
  /** 0 turns the timer off. */
  intervalMinutes: number;
  syncOnChange: boolean;
  /** How many files are compared and transferred at the same time. */
  parallelFiles: number;
};

export const DEFAULT_PARALLEL_FILES = 8;
export const MAX_PARALLEL_FILES = 64;

/** The OAuth client registered for one server, and its tokens while signed in. */
export type StoredAuth = {
  origin: string;
  clientId: string;
  endpoints: OAuthEndpoints;
  tokens: TokenSet | null;
};

/** A sign-in waiting for the browser to redirect back. Kept on disk in case the app restarts. */
export type PendingSignIn = {
  origin: string;
  clientId: string;
  endpoints: OAuthEndpoints;
  verifier: string;
  state: string;
};

export type LastSync = { at: number; ok: boolean; text: string };

export type PluginData = {
  settings: Settings;
  auth: StoredAuth | null;
  pendingSignIn: PendingSignIn | null;
  /** The server `syncState` describes. State from one server must never be applied to another. */
  stateOrigin: string;
  syncState: SyncState;
  lastSync: LastSync | null;
  /** The most recent run that actually moved something, so it is not hidden by later idle runs. */
  lastChange: LastSync | null;
};

/** Reads the saved data file, falling back to defaults for anything missing or malformed. */
export function parsePluginData(raw: unknown, defaultDeviceName: string): PluginData {
  const root = isRecord(raw) ? raw : {};
  const settings = isRecord(root["settings"]) ? root["settings"] : {};
  return {
    settings: {
      serverUrl: stringField(settings, "serverUrl") ?? "",
      deviceName: stringField(settings, "deviceName") ?? defaultDeviceName,
      intervalMinutes: Math.max(0, numberField(settings, "intervalMinutes") ?? 5),
      syncOnChange: booleanField(settings, "syncOnChange") ?? true,
      parallelFiles: Math.min(MAX_PARALLEL_FILES, Math.max(1, Math.round(numberField(settings, "parallelFiles") ?? DEFAULT_PARALLEL_FILES))),
    },
    auth: parseAuth(root["auth"]),
    pendingSignIn: parsePendingSignIn(root["pendingSignIn"]),
    stateOrigin: stringField(root, "stateOrigin") ?? "",
    syncState: parseSyncState(root["syncState"]),
    lastSync: parseLastSync(root["lastSync"]),
    lastChange: parseLastSync(root["lastChange"]),
  };
}

function parseEndpoints(raw: unknown): OAuthEndpoints | null {
  if (!isRecord(raw)) return null;
  const authorizationEndpoint = stringField(raw, "authorizationEndpoint");
  const tokenEndpoint = stringField(raw, "tokenEndpoint");
  const registrationEndpoint = stringField(raw, "registrationEndpoint");
  if (authorizationEndpoint === null || tokenEndpoint === null || registrationEndpoint === null) {
    return null;
  }
  return { authorizationEndpoint, tokenEndpoint, registrationEndpoint };
}

function parseClient(
  raw: JsonRecord,
): { origin: string; clientId: string; endpoints: OAuthEndpoints } | null {
  const origin = stringField(raw, "origin");
  const clientId = stringField(raw, "clientId");
  const endpoints = parseEndpoints(raw["endpoints"]);
  if (origin === null || clientId === null || endpoints === null) return null;
  return { origin, clientId, endpoints };
}

function parseAuth(raw: unknown): StoredAuth | null {
  if (!isRecord(raw)) return null;
  const client = parseClient(raw);
  if (client === null) return null;
  const tokens = raw["tokens"];
  const accessToken = isRecord(tokens) ? stringField(tokens, "accessToken") : null;
  if (!isRecord(tokens) || accessToken === null) return { ...client, tokens: null };
  return { ...client, tokens: { accessToken, refreshToken: stringField(tokens, "refreshToken") } };
}

function parsePendingSignIn(raw: unknown): PendingSignIn | null {
  if (!isRecord(raw)) return null;
  const client = parseClient(raw);
  const verifier = stringField(raw, "verifier");
  const state = stringField(raw, "state");
  if (client === null || verifier === null || state === null) return null;
  return { ...client, verifier, state };
}

function parseSyncState(raw: unknown): SyncState {
  const state: SyncState = {};
  if (!isRecord(raw)) return state;
  for (const [path, entry] of Object.entries(raw)) {
    if (!isRecord(entry)) continue;
    const hash = stringField(entry, "hash");
    const mtime = numberField(entry, "mtime");
    const size = numberField(entry, "size");
    if (hash !== null && mtime !== null && size !== null) state[path] = { hash, mtime, size };
  }
  return state;
}

function parseLastSync(raw: unknown): LastSync | null {
  if (!isRecord(raw)) return null;
  const at = numberField(raw, "at");
  const ok = booleanField(raw, "ok");
  const text = stringField(raw, "text");
  return at === null || ok === null || text === null ? null : { at, ok, text };
}
