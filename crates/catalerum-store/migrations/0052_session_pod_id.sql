-- Multi-pod HA (SOUL §16 M7): owning-pod identity for node-local session rows.
--
-- Terminal PTYs and podman/kubernetes sandboxes are inherently pod-local — only
-- the process that opened them can drive them. Under the N-replica Deployment a
-- pod's boot reconcile must reclaim ONLY the rows it owns (plus legacy NULL rows
-- from before this column), never a peer pod's live-session rows. `pod_id`
-- records the owning process (CATALERUM_POD_ID env → HOSTNAME → random UUID,
-- resolved once at boot). NULL ⇒ a pre-upgrade row with no recorded owner
-- (reclaimed by whichever pod boots first — harmless, its live handle is gone).
--
-- Additive + nullable, so existing rows keep working unchanged. Boot reconcile
-- is the only reader and runs once per process on tiny tables, so no index is
-- warranted (a peer pod's rows are simply left alone — see the ownership docs).
ALTER TABLE terminal_sessions   ADD COLUMN pod_id TEXT;
ALTER TABLE workspace_sandboxes ADD COLUMN pod_id TEXT;
