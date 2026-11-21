/**
 * OAuth 2.0 authorization-code flow with PKCE (S256) for a public client:
 * metadata discovery (RFC 8414), dynamic client registration (RFC 7591), resource indicators
 * (RFC 8707), code exchange and refresh.
 */

import { type JsonRecord, parseJsonRecord, stringField } from "./guards";
import type { HttpFetch, HttpResponse } from "./remote";

export const REDIRECT_URI = "obsidian://context-broker-sync";
const CLIENT_NAME = "Context Broker Sync (Obsidian)";
const REQUEST_TIMEOUT_MS = 30_000;

export type OAuthEndpoints = {
  authorizationEndpoint: string;
  tokenEndpoint: string;
  registrationEndpoint: string;
};

export type TokenSet = {
  accessToken: string;
  /** Null when the server issued none; the user then signs in again when the token expires. */
  refreshToken: string | null;
};

export type PkceChallenge = { verifier: string; challenge: string; state: string };

export class OAuthError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "OAuthError";
  }
}

/** Reads the authorization server metadata published by the server origin. */
export async function discoverEndpoints(http: HttpFetch, origin: string): Promise<OAuthEndpoints> {
  const response = await http(`${origin}/.well-known/oauth-authorization-server`, {
    method: "GET",
    headers: { Accept: "application/json" },
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  const metadata = await jsonBody(response, "OAuth discovery");
  return {
    authorizationEndpoint: endpoint(metadata, "authorization_endpoint", origin),
    tokenEndpoint: endpoint(metadata, "token_endpoint", origin),
    registrationEndpoint: endpoint(metadata, "registration_endpoint", origin),
  };
}

/** Registers this plugin as a public client and returns its client id. */
export async function registerClient(http: HttpFetch, endpoints: OAuthEndpoints): Promise<string> {
  const response = await http(endpoints.registrationEndpoint, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json" },
    body: JSON.stringify({
      client_name: CLIENT_NAME,
      redirect_uris: [REDIRECT_URI],
      grant_types: ["authorization_code", "refresh_token"],
      response_types: ["code"],
      token_endpoint_auth_method: "none",
    }),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  const registration = await jsonBody(response, "OAuth client registration");
  const clientId = stringField(registration, "client_id");
  if (clientId === null || clientId === "") {
    throw new OAuthError("OAuth client registration returned no client_id.");
  }
  return clientId;
}

/** S256 code challenge for a verifier: base64url(sha256(verifier)). */
export async function challengeFor(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
  return base64Url(new Uint8Array(digest));
}

export async function createPkce(): Promise<PkceChallenge> {
  const verifier = base64Url(crypto.getRandomValues(new Uint8Array(32)));
  const state = base64Url(crypto.getRandomValues(new Uint8Array(16)));
  return { verifier, challenge: await challengeFor(verifier), state };
}

/** The URL to open in the browser. `resource` is the server origin (RFC 8707). */
export function authorizationUrl(
  endpoints: OAuthEndpoints,
  clientId: string,
  pkce: PkceChallenge,
  resource: string,
): string {
  const url = new URL(endpoints.authorizationEndpoint);
  url.searchParams.set("response_type", "code");
  url.searchParams.set("client_id", clientId);
  url.searchParams.set("redirect_uri", REDIRECT_URI);
  url.searchParams.set("code_challenge", pkce.challenge);
  url.searchParams.set("code_challenge_method", "S256");
  url.searchParams.set("state", pkce.state);
  url.searchParams.set("resource", resource);
  return url.toString();
}

export function exchangeCode(
  http: HttpFetch,
  endpoints: OAuthEndpoints,
  clientId: string,
  code: string,
  verifier: string,
  resource: string,
): Promise<TokenSet> {
  return requestTokens(http, endpoints, null, {
    grant_type: "authorization_code",
    code,
    redirect_uri: REDIRECT_URI,
    client_id: clientId,
    code_verifier: verifier,
    resource,
  });
}

export function refreshTokens(
  http: HttpFetch,
  endpoints: OAuthEndpoints,
  clientId: string,
  refreshToken: string,
  resource: string,
): Promise<TokenSet> {
  // Servers that do not rotate refresh tokens omit it from the response: keep the current one.
  return requestTokens(http, endpoints, refreshToken, {
    grant_type: "refresh_token",
    refresh_token: refreshToken,
    client_id: clientId,
    resource,
  });
}

async function requestTokens(
  http: HttpFetch,
  endpoints: OAuthEndpoints,
  fallbackRefreshToken: string | null,
  form: Record<string, string>,
): Promise<TokenSet> {
  const response = await http(endpoints.tokenEndpoint, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: "application/json" },
    body: new URLSearchParams(form).toString(),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  const tokens = await jsonBody(response, "OAuth token request");
  const accessToken = stringField(tokens, "access_token");
  if (accessToken === null || accessToken === "") {
    throw new OAuthError("OAuth token response contained no access_token.");
  }
  return { accessToken, refreshToken: stringField(tokens, "refresh_token") ?? fallbackRefreshToken };
}

async function jsonBody(response: HttpResponse, action: string): Promise<JsonRecord> {
  const body = parseJsonRecord(await response.arrayBuffer());
  if (response.status < 200 || response.status >= 300) {
    // The error code is safe to show; the rest of the body is not included on purpose.
    const code = body === null ? null : stringField(body, "error");
    throw new OAuthError(`${action} failed with HTTP ${response.status}${code ? ` (${code})` : ""}.`);
  }
  if (body === null) throw new OAuthError(`${action} did not return JSON.`);
  return body;
}

/** Tokens and codes must only ever go to HTTPS endpoints (plain HTTP is for local testing). */
function endpoint(metadata: JsonRecord, key: string, origin: string): string {
  const value = stringField(metadata, key);
  if (value === null) throw new OAuthError(`OAuth discovery is missing "${key}".`);
  let url: URL;
  try {
    url = new URL(value);
  } catch (error) {
    if (!(error instanceof TypeError)) throw error;
    throw new OAuthError(`OAuth discovery has an invalid "${key}".`);
  }
  const local = url.hostname === "localhost" || url.hostname === "127.0.0.1";
  if (url.protocol !== "https:" && !(local && url.origin === origin)) {
    throw new OAuthError(`OAuth endpoint "${key}" is not HTTPS.`);
  }
  return url.toString();
}

function base64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
