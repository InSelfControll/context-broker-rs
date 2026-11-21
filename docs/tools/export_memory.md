# `export_memory`

Copies what the default JSON store holds, chat sessions and saved search results, into an Obsidian vault or into Graphify memory docs. Use it when you start using one of them and want the history you already have to be there. The JSON store is left as it is.

Call it only when the user asks. It can be repeated: messages and results already present are skipped, so a later run sends only what was recorded since.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `target` | `obsidian` or `graphify` | no | detected: a vault if one is configured or found, else Graphify if the project has `graphify-out/` or the CLI is installed |
| `all_projects` | boolean | no | `false` (only this project) |
| `project_root` | string | no | auto-detected |

## Where it goes

- **Obsidian:** the vault's own `AI Chats/` structure when it has one (`AI Chats/Chats/Context Broker/...`, listed under its Source and Project notes), else `<vault>/Context Broker/<project>-<digest>/Chats/<session>.md` and `Results/<name>.md`. The vault is `CONTEXT_BROKER_OBSIDIAN_VAULT`, or the one Obsidian itself lists as open or most recently used.
- **Graphify:** `<project>/graphify-out/memory/`, one doc per exchange plus `result_<name>.md` per saved result. Graphify folds them into the graph on its next update.

Messages keep their original timestamps.

## Example

```json
{ "all_projects": true }
```

```text
Exported to Obsidian vault /home/me/Documents/Vault: 42 new message(s) from 9 session(s), 3 new saved result(s), 2 project(s). 1 stored project(s) skipped: folder no longer exists.
```

Nothing to export to: error `no Obsidian vault or Graphify found on this machine; ...`.

To keep recording there from now on, set `CONTEXT_BROKER_STORAGE` to `obsidian`, `graphify` or `auto`.

[Back to all tools](../TOOLS.md)
