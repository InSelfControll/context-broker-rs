# OAuth clients: redirect addresses, client IDs and secrets

When the broker is published behind a Cloudflare Access application with Managed OAuth (see
[REMOTE.md](REMOTE.md#access-application)), Access is the OAuth server the apps sign in through.
Most apps register themselves automatically (dynamic client registration). This page covers the two
things that go wrong: a redirect address Access does not allow, and an app that asks you for a
client ID and client secret.

The examples use `vault.example.com` for the broker's hostname and `<team>.cloudflareaccess.com` for
your Access team domain.

| What | Where |
|---|---|
| Where to sign in, as the broker announces it | `https://vault.example.com/.well-known/oauth-protected-resource` |
| OAuth server details | `https://<team>.cloudflareaccess.com/.well-known/oauth-authorization-server` |
| Registration endpoint | `https://<team>.cloudflareaccess.com/cdn-cgi/access/oauth/registration` |

## 1. Allow the app's redirect address

Every registration is checked against the redirect addresses allowed on the Access application.
An address that isn't on the list fails with:

```json
{"error":"invalid_client_metadata","error_description":"redirect_uri is not allowed by the account configuration"}
```

Despite the wording, the list belongs to the **Access application**, not to the account: Zero
Trust → Access → Applications → your application → Advanced settings → Managed OAuth → dynamic
client registration.

- `https://` addresses may end in `/*` to allow everything below that path.
- Custom-scheme addresses (such as `obsidian://context-broker-sync`) must match exactly.
- "Allow any on localhost" and "Allow any on loopback" cover clients running on your own machine.

| App | Redirect address |
|---|---|
| Claude | `https://claude.ai/api/mcp/auth_callback` and `https://claude.com/api/mcp/auth_callback` |
| ChatGPT | `https://chatgpt.com/connector_platform_oauth_redirect`, `https://chatgpt.com/connector/oauth/*` |
| Google Gemini (custom MCP connector) | `https://oauth-redirect.googleusercontent.com/r/user_bound_custom-mcp-<id>`. The `<id>` belongs to one connector; adding the connector again can give a new one. To allow any Google connector, use `https://oauth-redirect.googleusercontent.com/r/*` |
| Obsidian plugin in this repository | `obsidian://context-broker-sync` |
| Claude Code, Codex and other local clients | no address to list: turn on "Allow any on localhost" and "Allow any on loopback" |

### With the API instead of the dashboard

Needs an API token with *Access: Apps and Policies Edit*. The PUT replaces the whole application,
so send back everything you read, changing only the list:

```bash
ACCOUNT_ID=...   # Cloudflare account ID
APP_ID=...       # the Access application's ID
NEW_URI='https://example.com/oauth/callback'
API="https://api.cloudflare.com/client/v4/accounts/$ACCOUNT_ID/access/apps/$APP_ID"

curl -s "$API" -H "Authorization: Bearer $CF_API_TOKEN" \
| jq --arg u "$NEW_URI" '.result
    | del(.id, .aud, .created_at, .updated_at)
    | .oauth_configuration.dynamic_client_registration.allowed_uris |= ((. + [$u]) | unique)' \
| curl -s -X PUT "$API" -H "Authorization: Bearer $CF_API_TOKEN" \
    -H 'Content-Type: application/json' --data @- \
| jq '.result.oauth_configuration.dynamic_client_registration.allowed_uris'
```

## 2. Make a client ID and client secret

Some apps fall back to asking for credentials when automatic registration fails. Google, for
example, says *"Automatic registration with this server failed. Enter your OAuth client ID and
client secret to connect."* You can make them yourself with the same registration endpoint.

The redirect address must already be allowed (step 1):

```bash
REDIRECT_URI='https://oauth-redirect.googleusercontent.com/r/user_bound_custom-mcp-<id>'

curl -s -X POST "https://<team>.cloudflareaccess.com/cdn-cgi/access/oauth/registration" \
  -H 'Content-Type: application/json' \
  -d "$(jq -n --arg r "$REDIRECT_URI" '{
        client_name: "Google (context broker)",
        redirect_uris: [$r],
        grant_types: ["authorization_code", "refresh_token"],
        response_types: ["code"],
        token_endpoint_auth_method: "client_secret_post"
      }')" | jq
```

A `201` reply contains `client_id` and `client_secret`. Enter both in the app. If the app sends
its credentials in an `Authorization` header instead, register with `client_secret_basic`; Access
accepts either.

The secret is shown **once**. Keep it in a password manager; never commit it, or paste it into
chats, issues or logs. If it leaks, register a new client and use that one.

## 3. When it still fails

- **`redirect_uri is not allowed` at registration:** the address differs from the allowed one.
  Matching is exact: a trailing `/` or a different connector ID counts as a different address.
  To see what the app really sends, open the browser's developer tools (Network tab), connect
  again, and look at `redirect_uris` in the request to `/cdn-cgi/access/oauth/registration`.
- **Redirect error after signing in, with a client you made by hand:** the app is now using a
  different redirect address. Copy it from the error page's URL, allow it, and make a new client.
- **Access refuses you after signing in:** your account doesn't match the application's Allow
  policy. Fix the policy; the OAuth settings are fine.
- **Quick check of an address:** `{"redirect_uris":["<address>"]}` posted to the registration
  endpoint returns `201` when it is allowed. Every successful post creates a client, so don't run
  checks in a loop.
