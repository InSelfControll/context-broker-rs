# `query_graph`

Reads the project's [Graphify](https://github.com/Graphify-Labs/graphify) knowledge graph (`graphify-out/graph.json`). With `node` alone it returns that node and its connections; with `to` it returns the shortest path between the two nodes. The broker never builds the graph: run `/graphify .` in your assistant or `graphify extract .` first.

The tool is listed when `CONTEXT_BROKER_STORAGE=graphify`, or when the project the server starts in already has a graph.

## Parameters

| Name | Type | Required | Default |
|------|------|----------|---------|
| `node` | string | yes | - |
| `to` | string | no | none (connections of `node`) |
| `limit` | integer | no | 20 connections |
| `project_root` | string | no | auto-detected (see below) |

`node` and `to` accept a node id, a label (case is ignored), part of a label, or a short question. `project_root` falls back to `CONTEXT_BROKER_PROJECT_ROOT`, then to marker-based detection from the working directory, then to the working directory.

## Examples

Connections of a node (most connected neighbours first; `-->` leaves the node, `<--` arrives at it):

```json
{ "node": "PaymentGateway" }
```

```text
PaymentGateway · src/gateway.py L1 · community 2 · 3 edges
--> SessionStore · src/sessions.py L1 [uses]
<-- charge_card() · src/billing.py L1 [calls]
<-- refund_payment() · src/billing.py L1 [calls, INFERRED]
```

Edges Graphify read directly from the source carry only the relation; inferred ones show their confidence tag. When more connections exist than `limit`, the reply ends with `+N more (raise limit)`.

Shortest path:

```json
{ "node": "login_user()", "to": "refund_payment()" }
```

```text
3 hop(s): login_user() --calls--> SessionStore <--uses-- PaymentGateway <--calls-- refund_payment()
```

Several nodes match:

```text
2 nodes match 'pay'; pass one id as node:
gateway — PaymentGateway · src/gateway.py L1
billing_refund — refund_payment() · src/billing.py L1
```

Other replies: `No node matches '<name>'.`, `No path between <a> and <b>.`. No graph in the project: error `no Graphify graph at <path>; build one with \`graphify extract .\``.

[Back to all tools](../TOOLS.md)
