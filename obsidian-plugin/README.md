# Context Broker Sync

An Obsidian plugin (desktop and Android) that keeps a vault in sync with a Context Broker server
over HTTPS, using the server's `/sync` API.

## What it does

- Syncs on a timer, a few seconds after a note changes, from the ribbon icon, or with the command
  **Sync now**. One sync runs at a time; a change during a sync causes one more run afterwards.
- Three-way sync against the state recorded at the last sync, so it knows which side changed.
- When both sides changed the same file differently, nothing is lost: the server version keeps
  the path and this device's version is saved next to it as
  `Note (conflict <device name> <YYYY-MM-DD HHmmss>).md` and uploaded.
- Files whose size and modification time are unchanged are not read or hashed again.
- Never syncs paths with a component starting with `.` (`.obsidian/`, `.git/`, `.trash/`) or files
  over 50 MB.
- Files deleted by sync go through Obsidian's own trash setting.
- If the server suddenly lacks most of the synced files (more than 10 and more than half), the
  sync stops and changes nothing. Run **Sync now and accept files deleted on the server** to
  confirm that the deletion is real.

## Server

Start the broker in HTTP mode with `CONTEXT_BROKER_SYNC=1`. The plugin talks to
`<server>/sync/manifest` and `<server>/sync/file`.

Behind Cloudflare Access with Managed OAuth: on a trusted network no sign-in is needed. Elsewhere,
open the plugin settings and choose **Sign in**; the browser opens, and after you approve, it
returns to Obsidian through `obsidian://context-broker-sync`. Tokens are stored in the plugin's
`data.json` inside `.obsidian/`, which is never synced.

## Build

```sh
bun install
bun run build       # writes main.js (CommonJS; obsidian and electron stay external)
bunx tsc --noEmit
```

## Install into a vault

Copy two files into `<vault>/.obsidian/plugins/context-broker-sync/`:

- `main.js` (built by `bun run build`)
- `manifest.json`

There is no `styles.css`. The plugin creates the third file in that folder, `data.json`
(settings, tokens and sync state), on first use. Then enable **Context Broker Sync** under
Settings, Community plugins, and set the server URL. On Android the folder is the same path
inside the vault on the device.

## Test

```sh
bun test
```

`tests/e2e.test.ts` is the one end-to-end test. It starts the real broker binary from
`../target/release/context-broker` on a free local port and syncs two simulated devices (real
folders) through it. The OAuth sign-in flow needs a browser and is not covered by it.

## Layout

| File | Role |
|---|---|
| `src/engine.ts` | Sync engine over the `FileStore` and `Remote` interfaces; no Obsidian, no HTTP |
| `src/remote.ts` | `Remote` over an injected `fetch`-shaped function; bearer token, refresh on 401 |
| `src/oauth.ts` | Discovery, dynamic registration, PKCE, authorization URL, code exchange, refresh |
| `src/obsidian-io.ts` | `FileStore` on the vault adapter and `fetch` shape on `requestUrl` |
| `src/data.ts` | Shape and loading of the persisted plugin data |
| `src/main.ts` | The plugin: settings tab, status bar, ribbon, commands, timers, sign-in |
