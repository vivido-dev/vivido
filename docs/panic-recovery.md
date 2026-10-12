# Panic containment and client recovery

Vivido deliberately retains in-process client containment as a project exception to
M-PANIC-CONTINUATION and M-PANIC-IS-STOP. Restarting an embedding host would also terminate
unrelated panes and owners. This exception applies only to the boundaries below; it is not a
claim that arbitrary callbacks, native faults, or partially mutated shared state can recover.
Ordinary malformed input remains a validated error, not a panic.

A contained panic stops the affected operation and retires its resources. Its payload is never
copied into fault metadata. Recovery creates new state; a mutex becoming unlocked after unwind
is not evidence that its former contents are valid.

| Boundary | State retired after panic | Recovery |
| --- | --- | --- |
| PTY parser (`terminal/event_loop.rs`) | Parser buffers, queued writes, marker fragments; the host stops and joins the worker, replaces the terminal, closes pending clipboard prompts, and disconnects this pane's Vivid clients. | `restart_terminal` creates a fresh PTY, terminal, parser, and Vivid service. A quarantined pane rejects `reset_terminal`. |
| PTY writer (`terminal/event_loop.rs`) | Same pane and worker resources as parser failure. | Fresh client restart. |
| IPC reader (`polling/ipc.rs`) | The failing connection closes; its disconnect event removes associated requests, waiters, and subscriptions. | Open a new connection and handshake again. |
| IPC writer (`polling/ipc.rs`) | The failing connection closes and its socket shutdown wakes the reader. Queued frames cannot be sent on a replacement connection. | Open a new connection. |
| Vivid connection reader (`vivid/mod.rs`) | RAII guards retire the connection slot, session or track attachment, owner-scoped scene state, and transports. | Establish a new authenticated session or use the protocol's validated recovery procedure. |
| Vivid control actor (`vivid/mod.rs`) | Stops its control reader and egress; the reader's session cleanup retires the complete owner identity. | New authenticated session. |
| Vivid egress (`vivid/actor.rs`) | Discards the queued records, marks the egress closed, and stops its writer and attached reader. Poisoned queue storage is recovered solely for destruction. | New authenticated session or track transport. |

Terminal quarantine remains visible in `client_health` and `last_client_fault`. A healthy pane's
ordinary reset still clears client-controlled state, but cannot be used to resume an unwound
worker. Restart does not reset another pane's terminal or owner-scoped protocol resources.

The regressions inject partial terminal/parser/write state, partial IPC request state (for both
worker roles), and a poisoned egress queue. A live control-actor test removes an owner's context
before panicking, then verifies retirement and continued operation of another session using the
same local context and surface IDs. IPC tests reuse both connection and request IDs across
independent owners. Existing protocol lifecycle tests additionally cover owner-scoped attachment
and reader teardown. These checks support the bounded exception; they do not certify native
code against all possible failures. An uncontained panic remains an application failure.
