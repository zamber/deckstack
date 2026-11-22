# PR {{ID}} — {{TITLE}}

Filled by `sh tools/pr.sh submit {{ID}}` (worker, ticket branch) and
`sh tools/pr.sh review {{ID}}` / `merge {{ID}}` (steward, main). Copy this file per
PR; `prs/README.md` documents the lifecycle.

```json
{
  "ticket": "{{ID}}",
  "title": "{{TITLE}}",
  "owner": "{{OWNER}}",
  "branch": "{{BRANCH}}",
  "base": "{{BASE}}",
  "head": "{{HEAD}}",
  "submitted": "{{SUBMITTED}}",
  "verdict": "{{VERDICT}}",
  "reviewer": "{{REVIEWER}}",
  "reviewed": "{{REVIEWED}}",
  "merge_commit": "{{MERGE_COMMIT}}"
}
```

## Commits (base..head)

{{COMMITS}}

## Diffstat

{{DIFFSTAT}}

## Checks

{{CHECKS}}

## Acceptance commands and results

{{ACCEPTANCE}}

## Risks

{{RISKS}}

## Findings

{{FINDINGS}}

## Verdict

{{VERDICT_TEXT}}

## Merge

{{MERGE_TEXT}}
