# Automation backend architecture

The automation system is split into a control plane, dispatch plane, and execution
plane. The web editor is a client of the control plane; it is not part of the
runtime engine.

```text
Leptos editor / REST / LLM authoring tools
                  │
                  ▼
       automation definitions (Postgres)
                  │ immutable snapshot
trigger sources ──┴──► durable job queue
                         │
                         ▼
                 AutomationEngine
                 ├── ExecutionState ──► run journal + grant snapshot
                 ├── ActionRunner   ──► typed, capability-gated tools
                 └── CodeRunner     ──► sandboxed JS runtime
```

## Component contracts

### Control plane

The Leptos editor, automation REST routes, and automation authoring tools create,
validate, enable, and inspect definitions. Postgres is the source of truth. Control
plane code may use the typed graph/spec model from `catalerum-automation`, but it
must not call action or code runtimes.

### Dispatch plane

Trigger adapters, collectors, and the scheduler match enabled definitions and write
`run_automation` jobs. A job carries only the workspace, automation id, and trigger
payload. Dispatch does not execute actions in the request or scheduler process.

### Execution plane

`AutomationEngine` receives an immutable `Automation` snapshot and owns graph/linear
orchestration, branching, crash resumption, redelivery gates, and run finalization.
It depends on three ports:

- `ExecutionState`: open/resume runs, append/finalize steps, and resolve the grant
  snapshotted on the run.
- `ActionRunner`: perform typed, capability-gated effects.
- `CodeRunner`: evaluate sandboxed code and condition nodes.

The engine does not know HTTP, Leptos, SQL queries, job claiming, tool registries,
Boa, or provider names.

### Adapters

`PostgresExecutionState` is the current durable journal adapter. `ToolActionRunner`
and `ScriptCodeRunner` are the runtime adapters assembled by the binary. Tests can
replace each independently; the engine unit suite uses an in-memory execution-state
adapter and no database. Building `catalerum-automation` with
`--no-default-features` excludes the Postgres adapter and its store dependency
entirely.

## Dependency rules

1. Frontend and REST code may depend on domain/spec types, never executor adapters.
2. Trigger and queue producers may enqueue execution, never invoke an action inline.
3. The engine only calls its three ports.
4. Storage adapters implement engine ports; engine code never calls a repository
   directly.
5. The binary is the composition root. It is the only place that should choose the
   concrete state, action, and code adapters for deployed workers.

These rules keep definition storage independently evolvable (schema/API/editor),
allow the execution engine to move to a dedicated worker service without changing
the frontend, and retain Postgres as the durable source of truth.
