## Automation

Use an automation job when the user wants work to run on a recurring cron schedule or wants a named task that can be triggered later. Do not create a persistent job for a one-off task unless the user asks for one.

- List jobs: `dwo automation --project PROJECT_ID list`.
- Inspect one job's schedule, enabled state, effective model, bound session, active runs, and recent results: `dwo automation --project PROJECT_ID status JOB`.
- Add an enabled job: `dwo automation --project PROJECT_ID add JOB --cron "EXPR" --prompt "INSTRUCTION" [--topic TOPIC_ID] [--timezone ZONE] [--session every-time|once|fixed] [--session-id SESSION_ID] [--title TITLE]`. Add `--disabled` when it should not be scheduled yet.
- Enable or disable scheduling: `dwo automation --project PROJECT_ID enable JOB`, `dwo automation --project PROJECT_ID disable JOB`, or use `--all` when the user explicitly requests every job.
- Delete a job: `dwo automation --project PROJECT_ID delete JOB`. `dwo automation delete --all --yes` is destructive and requires an explicit request covering all jobs.
- Run a job immediately: `dwo automation --project PROJECT_ID run JOB`. Manual runs are allowed even while the job is disabled.

Jobs belong to a Project and use its workspace; do not add a cwd to the automation command. After adding or changing a job, use `dwo automation --project PROJECT_ID status JOB` to verify the stored cron, timezone, topic, session behavior, enabled state, and next run. Run it manually only when immediate execution is requested or is an intended and safe verification step. `automation run` returns after the target session is resolved or created and the prompt is submitted; it reports the run, session, and turn IDs but does not wait for completion. Use `automation status JOB` for an explicit progress check when needed.

When a manual run started from an agent session completes, fails, or is cancelled, its result is delivered automatically as an internal `<automation_result>` message. Do not busy-poll solely to detect completion.
