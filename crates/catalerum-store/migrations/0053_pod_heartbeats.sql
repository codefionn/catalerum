-- Pod heartbeats (multi-pod HA follow-up, SOUL §20/§16 M7): a liveness signal so
-- a permanently-dead pod's node-local terminal/sandbox rows self-heal.
--
-- WHY THIS EXISTS. `0052_session_pod_id` made boot reconcile pod-scoped so a
-- rolling restart no longer stomps a peer's live rows — but it left a hole: under
-- the shipped Deployment, pod names are random per ReplicaSet, so a replaced pod
-- NEVER returns under the same HOSTNAME. Its `active` terminal_sessions /
-- non-stopped workspace_sandboxes rows would then linger forever (the common
-- production case, not an edge), because "is that pod still alive?" was
-- unknowable. This table answers it: every live process upserts `(pod_id, now())`
-- on an interval (~30 s), and a periodic sweep reclaims rows whose owning pod has
-- a heartbeat that has gone stale (older than a generous grace ≫ the interval).
--
-- SAFETY RULE (documented on `reclaim_stale_for_dead_pods` in repo.rs): the sweep
-- reclaims a row only when its `pod_id` HAS a heartbeat row that is now stale — it
-- never reclaims a pod_id with NO heartbeat row at all. A pod running pre-heartbeat
-- code (during a rolling upgrade) never writes a heartbeat, so its still-live rows
-- must not be swept out from under it; leaving never-heartbeated pods to the legacy
-- NULL/boot-reconcile path is the conservative, provably-safe choice.
--
-- Tiny table (one row per live process); PK lookup + a range scan on `last_seen`
-- over a handful of rows, so no extra index is warranted. `last_seen` defaults to
-- now() so a plain upsert stamps it.
CREATE TABLE pod_heartbeats (
    pod_id    TEXT        PRIMARY KEY,
    last_seen TIMESTAMPTZ NOT NULL DEFAULT now()
);
