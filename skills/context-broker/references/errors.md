# Error replies


An error reply starts with `Error:`. The common ones:

| Reply | What to do |
|---|---|
| `project_root must be inside <folder>` | Remote broker. Call again without `project_root`. |
| `all_projects is not available over HTTP` | Remote broker. Call `export_memory` without `all_projects`. |
| `no indexable files found in ...` | Wrong folder, or a project with no supported file types. Check the root in the message. |
| `No stored messages for session '<id>'.` | The id is wrong. Find it with `load_cross_session_context`. |
| `saved result not found: <name>` | Call `load_saved_results` without `filename` to list the names. |
| `no Graphify graph at <path>` | The user has to build the graph first (`graphify extract .`). |
| A recording is rejected for a secret | Rewrite the message without the secret value and record again. |
| `turns must be a list of {user, assistant} objects` | Pass `turns` as a list, not a string. |

A reply of `No matches`, `No related project history.` or `No stored messages match` is an answer, not an error. Try different words once, then tell the user nothing was found.

