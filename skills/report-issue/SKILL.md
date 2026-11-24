---
name: report-issue
description: Capture a reproducible deckstack issue bundle and file it as a ticket instead of only describing the problem in chat.
---

# report-issue

Use when deckstack behaviour disagrees with a contract, a tool fails, or an agent finds
an inconsistency during development.

1. Reproduce the issue and note the exact command.
2. Run `python3 tools/report-issue --out reports/<slug>.md` from the repo.
3. Add a ticket from `tickets/TEMPLATE.md`: evidence, expected vs actual, acceptance.
4. Attach the bundle path in the ticket. Keep the raw `ds` output in the bundle, not only
   in the chat.
5. If the issue is a contract/documentation gap, say which file is wrong and quote the
   contradictory clause.

`tools/report-issue` is the interim bundle tool. The native `ds report` verb is DS-108; use
it once it lands. A bug report without the bundle is incomplete.
