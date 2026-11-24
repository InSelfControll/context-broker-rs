# Memory, export and saved results

Memory is per project. A **session** is one named conversation; its `session_id` is the name you choose when you first record it.

## Before you start work on a problem

Call `lookup_project_history` with the problem in a sentence when the user reports a bug, asks "why did we", or returns to something that sounds like it has history. It returns up to three past exchanges that match by shared words or by meaning.

`No related project history.` is a normal answer; carry on. When it does return something, say so to the user and verify it against the code before relying on it.

## Recalling

- `load_cross_session_context`: `search_query` is matched as plain text (case-insensitive) across all sessions of the project, newest first. Use a distinctive word or two, not a sentence, because the whole string has to appear in the message. `top_k` defaults to 10 (up to 50).
- `load_chat_context`: one session by `session_id`, newest messages last. `limit` defaults to 20; `search_query` keeps only messages containing that text. Use it when you know which session you want, for example to continue yesterday's work.

If you do not know the session id, find it with `load_cross_session_context` first: each line of its reply starts with `[session/role]`.

## Recording

Record when the user asks you to save, remember or log the conversation, or when a decision was made that someone will need later (why an approach was chosen, what a tricky bug turned out to be, how something is deployed). Do not record after every reply: it fills the memory with noise and makes later lookups worse.

- `record_turn`: one exchange. `session_id`, `user_message`, `assistant_message`.
- `record_session`: a whole conversation in one call. `session_id` and `turns`, a list of `{"user": "...", "assistant": "..."}` objects.

How to write a good record:

- **Summarize your own replies.** Write what was found, decided and done, with the names, paths, versions and commands that matter. Leave out tool output and filler. A later lookup shows only a few hundred characters of each message, and exchanges over 2,000 characters are skipped by `lookup_project_history`, so short and specific is what gets found again.
- **Keep the user's messages close to what they said.** They are what a future question will resemble.
- **Choose a session id that says what the work was**, with a date when it helps: `auth-refactor`, `broker-deploy-2026-11-23`. Letters, digits, `-`, `_` and `.` are kept as they are.
- **Reuse the same id to add to a session.** Both tools append, so recording the later part of a conversation under the same id extends the same note.
- **Leave secrets out.** A message that contains something shaped like a secret (`API_KEY=...`, `Bearer ...`, a private key header) is rejected and nothing is saved. Describe the credential ("the Cloudflare token in the service's environment") instead of pasting it.

The reply is `Saved N message(s) to session '<id>'.`, where N counts messages, two per exchange.

Example:

```json
{
  "session_id": "broker-deploy-2026-11-23",
  "turns": [
    {
      "user": "Build fails on the server after pulling main.",
      "assistant": "candle-core 0.11.1 needs a newer Rust than the system's 1.92 (error E0133). Installed rustup with Rust 1.100 under /root/.cargo; release build and tests pass."
    },
    {
      "user": "Install it on 10.0.40.6 too.",
      "assistant": "Copied the binary, kept the old one as context-broker-rs.bak, restarted context-broker.service. Ping returns a result; sha256 matches the local build."
    }
  ]
}
```

## Where recordings go

The user's configuration decides: a JSON store on the machine (the default), notes in an Obsidian vault, or Graphify memory docs. The tools and their replies are the same for all three, so you do not choose.

When the store is Obsidian, `record_turn` and `record_session` write the note directly. One session is one note, under `AI Chats/Chats/Context Broker/` when the vault has an `AI Chats` folder, else under `Context Broker/`.

## "Push this chat to Obsidian"

This is the request people most often get wrong, so follow these steps:

1. Call `record_session` with the conversation, summarized as described above. Leave `project_root` out.
2. Call `export_memory` once, with `target: "obsidian"`.
3. Read the reply:
   - `Exported to Obsidian vault <path>: N new message(s) ...` with N above zero: the history was copied into the vault.
   - `0 new message(s) from 0 session(s)`: this is not a failure. It means the broker already stores straight into the vault, so step 1 wrote the note and there was nothing left to copy. Tell the user the conversation is saved and give the session id.
   - `no Obsidian vault or Graphify found on this machine`: no vault is configured. Tell the user; the conversation is still saved in the broker's own store.

Then stop. Calling `export_memory` again adds nothing, because anything already present is skipped.

`export_memory` details:

- `target`: `obsidian` or `graphify`. Leave it out to let the server detect one.
- `all_projects` (default false): copies every project's history, not only this one. Use it only when the user asks for all projects. A remote broker refuses it (`all_projects is not available over HTTP`); the default call is the right one there.
- Call it only when the user asks. It is for moving stored history into a vault or graph, not something to run after each recording.

## Saved search results

Use these when a search reply is worth keeping: a map of where a feature lives, the files a long task will keep returning to.

- `save_search_results`: runs a semantic search (`query`, optional `top_k`) and stores the reply under `filename`. Pass a short plain name such as `auth-flow`; the extension is added for you.
- `list_saved_results`: the names saved for this project.
- `load_saved_results`: the original query on the first line, then the saved reply. Pass the same `filename`.

A saved result is a snapshot. Line numbers in it go stale as the code changes, so search again when it is old.

