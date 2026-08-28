No code-style conventions established yet (scaffold only, single `println!` in `main.rs`).

Process conventions (from `.claude/project/`):
- **Issue tracker**: GitHub issues in `ranarn/gameframe`, tracked on GitHub Project #4. Use `gh`
  CLI for all issue ops (create/view/list/comment/label/close) — patterns and the board's field
  IDs (`statusFieldId`, `priorityFieldId`, status/priority option IDs) are in
  `.claude/project/project-config.json`; load via
  `$cfg = Get-Content '.claude/project/project-config.json' | ConvertFrom-Json`. If that file is
  ever missing, run `/project-setup` to regenerate it — don't guess the IDs.
- **Triage labels**: five canonical roles map 1:1 to literal label strings in this repo:
  `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See
  `.claude/project/triage-labels.md` if the mapping ever changes.
- **Domain docs**: single-context layout planned (`CONTEXT.md` + `docs/adr/` at repo root), created
  lazily by `/grill-me` — neither exists yet, don't flag/create proactively.
- **Full design doc lives outside repo**: `../Project Ideas/gameframe.md` (sibling dir to this
  repo, not tracked here) has the full problem statement and design decisions — check there for
  rationale before making architecture calls.
- Repo also imports `~/.claude/CODING.md` (global, user-level) for the AFK pipeline
  (`/grill-me` → `/triage` → `/start` → `/review`) and installed-skills index.
