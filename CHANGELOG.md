# Changelog

After an update, leaf shows the current version's notes once, over whatever
it opens first. Each release needs a `## <version>` section with one line
per change, starting with `- `.

## Unreleased

- Pull requests have a Checks tab: every check by workflow with its duration, any Actions job's steps and log, and GitHub's re-runs: failed jobs, one job, or all jobs. The page refreshes while checks run.

## 0.15.2

- A calmer home screen: a greeting, today's reading time, the search and your documents, without the heat map, meters or fills.
- Recent documents in the folder you started in no longer repeat its path.

## 0.15.1

- The dithered look is now off by default. Turn it on with `leaf ui dither`, or "Leaf: Dither UI" in Rex's palette.
- After an update, leaf shows what changed once, like this.
- `leaf upgrade` and `leaf rex install` show each step as it runs, with download progress.

## 0.15.0

- A home screen: run `leaf` with nothing named to see today's reading time, your streak, a heat map and recent documents.
- leaf follows Rex theme switches at once.
- Each theme dithers in its own accent colors.
- The pull request header can be selected and copied like the body.
