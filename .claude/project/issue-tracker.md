# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues. Use the `gh` CLI for all operations.

## GitHub Project — Field IDs

Board field IDs are in `.claude/project/project-config.json` (written by `/project-setup`). Load them with:

```powershell
$cfg = Get-Content '.claude/project/project-config.json' | ConvertFrom-Json
```

Keys: `owner`, `projectId`, `projectNumber`, `statusFieldId`, `priorityFieldId`, `status.{backlog,ready,inProgress,inReview,done}`, `priority.{high,medium,low}`.

If `project-config.json` is missing, run `/project-setup` to generate it.

### Reference table

| Key | Value |
| --- | --- |
| owner | ranarn |
| ownerType | User |
| projectId | PVT_kwHOAFX4d84BeEwv |
| projectNumber | 4 |
| statusFieldId | PVTSSF_lAHOAFX4d84BeEwvzhYhqdU |
| priorityFieldId | PVTSSF_lAHOAFX4d84BeEwvzhYhq3o |
| status.backlog | f75ad846 |
| status.ready | 6f65eb6f |
| status.inProgress | 47fc9ee4 |
| status.inReview | b70162cf |
| status.done | 98236657 |
| priority.high | 49a66a73 |
| priority.medium | d0dffaeb |
| priority.low | 5dcc343d |

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, filtering comments by `jq` and also fetching labels.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply / remove labels**: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`
- **Close**: `gh issue close <number> --comment "..."`

Infer the repo from `git remote -v` — `gh` does this automatically when run inside a clone.

## When a skill says "publish to the issue tracker"

Create a GitHub issue.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.
