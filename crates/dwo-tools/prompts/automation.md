## Automation

Use an automation job when the user wants work to run on a recurring cron schedule or wants a named task that can be triggered later. Do not create a persistent job for a one-off task unless the user asks for one.

- List jobs: `dwo automation list`.
- Inspect one job's schedule, enabled state, effective model, bound session, active runs, and recent results: `dwo automation status JOB`.
- Add an enabled job: `dwo automation add JOB --cron "EXPR" --prompt "INSTRUCTION" [--timezone ZONE] [--session every-time|once|fixed] [--session-id SESSION_ID] [--cwd PATH] [--title TITLE]`. Add `--disabled` when it should not be scheduled yet.
- Enable or disable scheduling: `dwo automation enable JOB`, `dwo automation disable JOB`, or use `--all` when the user explicitly requests every job.
- Delete a job: `dwo automation delete JOB`. `dwo automation delete --all --yes` is destructive and requires an explicit request covering all jobs.
- Run a job immediately: `dwo automation run JOB`. Manual runs are allowed even while the job is disabled.

After adding or changing a job, use `dwo automation status JOB` to verify the stored cron, timezone, session behavior, enabled state, and next run. Run it manually only when immediate execution is requested or is an intended and safe verification step. `automation run` returns after the target session is resolved or created and the prompt is submitted; it reports the run, session, and turn IDs but does not wait for completion. Use `automation status JOB` for an explicit progress check when needed.

When a manual run started from an agent session completes, fails, or is cancelled, its result is delivered automatically as an internal `<automation_result>` message. Do not busy-poll solely to detect completion.
