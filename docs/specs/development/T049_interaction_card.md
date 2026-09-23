# T049: Command Approval Card

**Status:** Development

**Related:**
[T047: User Interaction Harmonization](../done/T047_user_interaction_harmonization.md)

## Summary

A `run_terminal` approval is the three-row card below. Enter confirms the highlighted row. Esc cancels. Option 2 remembers that exact command for this project and does not approve a longer or different shell string.

## Scope

Command approval text, keys, plain and one-shot lines, and the project-local exact-command file `.nib/command-prefixes.json`.

## Non-Goals

Question cards, non-shell Yes/No/View details cards, workspace consent, session-switch confirmation, recovery Continue, and the waiting-for-input status remain on their current T047 behavior in this increment.

## Card

`› ` is U+203A plus one space. Other rows start with two spaces. The footer is `Press enter to confirm or esc to cancel`.

```text
Would you like to run the following command?

Environment: local

Reason: May I run the focused interaction gate outside the sandbox to validate the corrected typing-cache timing and signal test?

$ task test:interactive

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for the exact command `task test:interactive` (p)
  3. No, and tell Codex what to do differently (esc)

Press enter to confirm or esc to cancel
```

Option 3 keeps the word Codex so the row matches the requested card.

## Decisions

- Yes starts highlighted. Enter or `y` on that row grants once and does not write the file.
- `y` or `p` on another row only moves the highlight. The letter grants only when that row is already highlighted.
- Option 2 is offered only for a command that can be stored safely: no shell metacharacter, more than one token, at most 1024 bytes, and not under a require-approval rule.
- Remembering stores the exact raw command plus cwd, background, timeout, verification id, plan id, affected paths, and max output bytes. `task test:interactive` does not match `task test:interactive --lib`, `&&`, `|`, `;`, a newline, or a different plan id.
- The executor is the only writer. A failed write does not run the command and shows `Input error: the command prefix was not saved`.
- Yes is offered only when the shown command is byte-identical to the raw command and contains no `[REDACTED]`. A fail-closed sandbox route does not prompt and does not honor a remembered command.
- Enter on No opens `Reason to record:`. Esc denies with no reason. Plain `esc`, `n`, and `no` deny immediately.
- Digits do not grant on this card.
- Other tool approvals still start on Deny.

## Affected Areas

- `src/interaction_card.rs`
- `src/tui/mod.rs`
- `src/tools/executor.rs`
- `src/tools/models.rs`
- `src/chat.rs`
- `src/console.rs`
- `docs/user/guide.md`
- `docs/tech/permissions.md`

## Acceptance Criteria

- [ ] The `task test:interactive` card matches the text above with a chevron, and without a chevron every row starts with two spaces. Option 2 contains one `(p)`.
- [ ] Enter or `y` on Yes grants once. Enter or `p` on the remember row asks the executor to store that exact invocation. Enter on No does not store or grant. Esc does not grant.
- [ ] A remembered `task test:interactive` runs that command again without a prompt and still prompts for `--lib`, `&&`, a newline, or a different `verification_id` or `plan_id`.
- [ ] A command that redacts to something other than itself never calls the approval handler and never runs.
- [ ] Plain `1` or `y` grants once, `2` or `p` remembers, `esc` denies, and `3` asks `Reason to record:`. An empty line retries.

## Implementation Plan

Implement the shared card and exact-command matching, wire the executor and interactive
handlers, then run the validation gates below.

## Validation Gates

- `task check`
- `task test:interactive`
- `task docs:check`

## Risks

Initial Enter now runs the visible command. The full command is on the card, and Esc cancels without running it.
