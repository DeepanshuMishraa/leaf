---
name: leaf-plan
description: Hand a written plan, spec or design to the person for review in leaf before acting on it. They note blocks of it and send a verdict, which arrives as your next message. Use after writing a plan that needs their sign-off, or when they ask to review your plan in leaf.
---

# leaf-plan: get a plan reviewed

1. Write the plan to a Markdown file, such as `docs/plans/<topic>.md`. Use headings for phases and list items for steps. Each heading section, list item, code block and table becomes one block the person can comment on, so one idea per item makes the review sharper. Don't also paste the plan into chat.
2. Run:

   ```bash
   leaf plan docs/plans/<topic>.md
   ```

   Inside Rex this opens the review beside your pane and returns at once with `Opened … for review`.
3. **End your turn.** Don't wait, sleep, poll or read the pane. The review arrives as your next message.

If leaf says there's nowhere to open it (not in Rex), tell the person to run `leaf plan docs/plans/<topic>.md` in their terminal. The review comes back to you the same way.

If you have to block until the answer, as in a script, run `leaf plan --wait <file>`. It prints the review to stdout when the person sends it.

## The review you get back

```markdown
# Review of docs/plans/auth.md: changes requested

Changes requested. Revise, then submit again.

Global: Split the migration into its own step.

## 1. Block 4, "Phase 2 > Tokens" (lines 12-18): comment
> Rotate the token on every privilege change

That logs people out mid-session.

## 2. Block 9 (line 30): remove
...
```

- **approved**: carry on with the plan as written.
- **approved with notes**: carry on, and apply every numbered item as you go.
- **changes requested**: revise the same file, addressing every numbered item. Then run `leaf plan` on it again for the next round. The person's notes on blocks you didn't change are kept.

The kinds of note:

- **comment**: do what it says.
- **remove**: take that block or passage out.
- **replace**: use the quoted text instead.

Line numbers refer to the file as you submitted it.

If the message says the review was closed without a verdict, ask the person how they'd like to go on.
