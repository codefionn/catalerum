-- catalerum-store — link an automation_run to the job_queue job that spawned it
-- (SOUL §5/§11/§6.2). A `run_automation` job re-driven by the §6.2 reconciler (a
-- worker that crashed mid-run) must RESUME its existing run rather than start a
-- fresh one and re-execute every action — otherwise a crash double-fires already-
-- completed side effects. `job_id` is the bridge: the engine records it on the run
-- and, on re-drive, finds the still-`running` run for the job and continues from
-- the first unfinished step. NULL for runs from a direct (non-job) invocation.
ALTER TABLE automation_runs ADD COLUMN job_id UUID;

-- Look up the active run for a job on re-drive (partial — only job-spawned runs).
CREATE INDEX automation_runs_job_idx ON automation_runs (job_id) WHERE job_id IS NOT NULL;
