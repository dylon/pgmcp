# pgmcp MCP Capabilities

The five MCP capabilities pgmcp registers (Tools, Resources, Completions,
Logging, Tasks). For tool semantics see [tool-catalog.md](tool-catalog.md).


pgmcp advertises 5 of 8 MCP capabilities:

| Capability      | Description                                                                   |
|-----------------|-------------------------------------------------------------------------------|
| **Tools**       | 71 tools across 9 capability tiers                                            |
| **Resources**   | 2 static resources + 3 resource templates with URI parameters                 |
| **Completions** | Auto-completion for resource template parameters (`{name}`, `{path}`)         |
| **Logging**     | Server-to-client log push with dynamic verbosity control via `set_level()`    |
| **Tasks**       | Long-running async operations (reindex) with progress tracking & cancellation |

### MCP Resources

| URI                | Description                        |
|--------------------|------------------------------------|
| `pgmcp://stats`    | Current indexing statistics (JSON) |
| `pgmcp://projects` | List of indexed projects (JSON)    |

### MCP Resource Templates

| URI Template                  | Parameter | Completable | Description                  |
|-------------------------------|-----------|-------------|------------------------------|
| `pgmcp://project/{name}`      | `name`    | Yes         | Project details by name      |
| `pgmcp://project/{name}/tree` | `name`    | Yes         | File tree for a project      |
| `pgmcp://file/{path}`         | `path`    | Yes         | Read an indexed file by path |

### Logging

The server pushes log messages to connected clients at the configured verbosity level.
Clients can change the level at any time via `logging/setLevel` (one of: `debug`, `info`,
`notice`, `warning`, `error`, `critical`, `alert`, `emergency`). Log events include
indexer progress, errors, and reindex lifecycle.

### Tasks

The `reindex` tool can be invoked as a long-running task via `tools/call` with the task
field set. The server returns a task ID immediately and processes the operation
asynchronously. Clients can poll `tasks/get` for progress, retrieve results via
`tasks/result`, list all tasks with `tasks/list`, or cancel with `tasks/cancel`.

---

