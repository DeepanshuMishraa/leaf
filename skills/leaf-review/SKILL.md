---
name: leaf-review
description: Hand your code changes to the person for review in leaf. They go file by file, note lines, files or the whole change, and send a verdict that arrives as your next message. Use when you've finished a change that needs their review, or when they ask to review your changes in leaf.
---

# leaf-review: get your changes reviewed

1. Finish the change and make sure it builds and its tests pass, so the review is about the change and not broken code.
2. Run one of these, matching what you changed:

   ```bash
   leaf review                 # unstaged changes
   leaf review --staged        # staged changes
   leaf review --branch        # this branch against its base
   leaf review main...HEAD     # any range
   leaf review https://github.com/owner/repo/pull/12
   ```

   Inside Rex this opens the review beside your pane and returns at once.
3. **End your turn.** Don't wait, sleep, poll or read the pane. The review arrives as your next message.

If leaf says there's nowhere to open it (not in Rex), tell the person to run the same command in their terminal. `leaf review --wait …` blocks and prints the review to stdout instead.

## The review you get back

The review is grouped by file in diff order. A whole-file note comes before that file's line notes, and each line note includes the hunk it's on:

~~~markdown
# Review of feat/tokens against main: changes requested

Global: Split the hook adapters into one file each.

## src/margin.rs

### 1. Whole file

The result file should be removed on every exit path.

### 2. Lines 40-52 (new side)
```diff
...
```
This leaks the file when the pane is closed.
~~~

- **approved**: the change is done.
- **approved with notes**: apply every numbered item, then you're done.
- **changes requested**: address every numbered item, then run `leaf review` again.

"Old side" line numbers refer to the file before the change, and "new side" to after.
