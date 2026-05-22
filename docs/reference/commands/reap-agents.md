---
description: Exit tracked agent panes older than a configured age
---

# reap-agents

Exit tracked agent panes whose last state update is older than a threshold.

```bash
workmux reap-agents
workmux reap-agents --hours 48
workmux reap-agents --hours 24 --force
```

By default, the command prints matching agents without exiting them. It still
reconciles workmux state while checking live panes. Pass `--force` to close
matching panes and remove their workmux agent state.

## Options

- `--hours <hours>`: Age threshold in hours. Defaults to `24`.
- `-f, --force`: Actually exit matching agents instead of showing what would
  happen.

## Example output

```text
Would exit %12 in feature-a (25h, done, Update docs)
Run with -f/--force to exit these agents.
```
