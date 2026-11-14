//! GitHub pull requests through the GitHub CLI: the open ones that concern
//! you, one read in full (checks, reviews, conversation), and what you can do
//! to it (merge, close, reopen, mark ready, approve, comment).

use std::process::{Command, Stdio};
use std::sync::LazyLock;

use serde_json::Value;

use crate::github::Resource;

static GH: LazyLock<bool> = LazyLock::new(|| {
    Command::new("gh")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
});

#[derive(Debug, Clone, thiserror::Error)]
pub enum PrFailed {
    #[error(
        "Pull requests are read through the GitHub CLI (`gh`), which isn't installed. Install it from https://cli.github.com, run `gh auth login`, then try again."
    )]
    NoGh,
    #[error("`gh {command}` failed: {cause}")]
    Gh { command: String, cause: String },
    #[error(
        "`gh {command}` answered with something leaf couldn't read ({cause}). Update gh, then try again."
    )]
    Unreadable { command: String, cause: String },
    #[error(
        "{target} isn't a pull request. Use a number (12), owner/repo#12, or a link like https://github.com/owner/repo/pull/12."
    )]
    BadTarget { target: String },
    #[error(
        "#{number} needs a repository, and this folder isn't a GitHub repository. Use owner/repo#{number} or the pull request's link."
    )]
    NoRepo { number: u64 },
}

/// One pull request: the repository (`owner/name`) and its number.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrRef {
    pub repo: String,
    pub number: u64,
}

impl PrRef {
    /// Reads `12`, `#12`, `owner/repo#12` or a pull request link; a bare
    /// number belongs to `here`, the repository of the current folder.
    pub fn parse(target: &str, here: Option<&str>) -> Result<Self, PrFailed> {
        let target = target.trim();
        let bad = || PrFailed::BadTarget {
            target: target.to_owned(),
        };
        if let Some(Resource::Pull {
            owner,
            repo,
            number,
        }) = Resource::parse_url(target)
        {
            return Ok(PrRef {
                repo: format!("{owner}/{repo}"),
                number,
            });
        }
        if let Some((repo, number)) = target.split_once('#')
            && !repo.is_empty()
        {
            let number = number.parse().map_err(|_| bad())?;
            let mut parts = repo.split('/');
            let valid = matches!(
                (parts.next(), parts.next(), parts.next()),
                (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
            );
            return valid
                .then(|| PrRef {
                    repo: repo.to_owned(),
                    number,
                })
                .ok_or_else(bad);
        }
        let number: u64 = target.trim_start_matches('#').parse().map_err(|_| bad())?;
        let repo = here.ok_or(PrFailed::NoRepo { number })?;
        Ok(PrRef {
            repo: repo.to_owned(),
            number,
        })
    }

    pub fn label(&self) -> String {
        format!("{}#{}", self.repo, self.number)
    }

    pub fn url(&self) -> String {
        format!("https://github.com/{}/pull/{}", self.repo, self.number)
    }

    pub fn issue_url(&self) -> String {
        format!("https://github.com/{}/issues/{}", self.repo, self.number)
    }

    pub fn resource(&self) -> Resource {
        let (owner, repo) = self.repo.split_once('/').unwrap_or(("", &self.repo));
        Resource::Pull {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            number: self.number,
        }
    }

    pub fn checkout(&self) -> String {
        format!("gh pr checkout {}", self.number)
    }
}

/// A row in the list of pull requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub pr: PrRef,
    pub title: String,
    pub author: String,
    pub updated: String,
    pub draft: bool,
    /// How and when it ended; `None` while it's open.
    pub ended: Option<Ended>,
}

impl Entry {
    /// When the list orders it by: when it ended, else when it last changed.
    pub fn when(&self) -> &str {
        self.ended.as_ref().map_or(&self.updated, |e| &e.at)
    }
}

/// How a merged pull request or closed issue in the list ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub how: Ending,
    pub at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Merged,
    Completed,
    NotPlanned,
}

impl Ending {
    /// The verb for the row's line: "merged 3d ago".
    pub fn verb(self) -> &'static str {
        match self {
            Ending::Merged => "merged",
            Ending::Completed => "closed",
            Ending::NotPlanned => "closed as not planned",
        }
    }
}

/// Which rows a list tab shows: those still open, or those done with
/// (merged pull requests, closed issues).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Open,
    Done,
}

impl Status {
    /// The switch's label for `kind`: Open, then Merged or Closed.
    pub fn label(self, kind: Kind) -> &'static str {
        match (self, kind) {
            (Status::Open, _) => "Open",
            (Status::Done, Kind::Pull) => "Merged",
            (Status::Done, Kind::Issue) => "Closed",
        }
    }

    pub fn other(self) -> Status {
        match self {
            Status::Open => Status::Done,
            Status::Done => Status::Open,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    /// A pull request only.
    Draft,
    /// A pull request only.
    Merged,
    /// A pull request closed without merging.
    Closed,
    /// An issue closed as done.
    Completed,
    /// An issue closed as not planned.
    NotPlanned,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Open => "Open",
            State::Draft => "Draft",
            State::Merged => "Merged",
            State::Closed | State::Completed | State::NotPlanned => "Closed",
        }
    }

    pub fn is_open(self) -> bool {
        matches!(self, State::Open | State::Draft)
    }
}

/// Why an issue is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Completed,
    NotPlanned,
}

impl CloseReason {
    pub const ALL: [CloseReason; 2] = [CloseReason::Completed, CloseReason::NotPlanned];

    pub fn label(self) -> &'static str {
        match self {
            CloseReason::Completed => "completed",
            CloseReason::NotPlanned => "not planned",
        }
    }

    pub fn key(self) -> char {
        match self {
            CloseReason::Completed => 'c',
            CloseReason::NotPlanned => 'n',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Squash => "Squash and merge",
            MergeMethod::Merge => "Merge",
            MergeMethod::Rebase => "Rebase and merge",
        }
    }

    pub fn key(self) -> char {
        match self {
            MergeMethod::Squash => 's',
            MergeMethod::Merge => 'm',
            MergeMethod::Rebase => 'r',
        }
    }

    fn flag(self) -> &'static str {
        match self {
            MergeMethod::Squash => "--squash",
            MergeMethod::Merge => "--merge",
            MergeMethod::Rebase => "--rebase",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Passed,
    Failed,
    Running,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub url: Option<String>,
}

/// What the checks add up to, as T3 Code words it.
pub fn describe_checks(checks: &[Check]) -> String {
    if checks.is_empty() {
        return "No checks".to_owned();
    }
    let total = checks.len();
    let count = |status| checks.iter().filter(|c| c.status == status).count();
    let (failed, running) = (count(CheckStatus::Failed), count(CheckStatus::Running));
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} of {total} running"));
    }
    if failed > 0 {
        parts.push(if parts.is_empty() {
            format!("{failed} of {total} failing")
        } else {
            format!("{failed} failed")
        });
    }
    if parts.is_empty() {
        let passed = count(CheckStatus::Passed);
        return if passed + count(CheckStatus::Skipped) == total {
            "All checks passed".to_owned()
        } else {
            format!("{passed} of {total} passing")
        };
    }
    parts.join(" · ")
}

/// The worst of the checks: failing beats running beats passing.
pub fn checks_status(checks: &[Check]) -> Option<CheckStatus> {
    if checks.is_empty() {
        None
    } else if checks.iter().any(|c| c.status == CheckStatus::Failed) {
        Some(CheckStatus::Failed)
    } else if checks.iter().any(|c| c.status == CheckStatus::Running) {
        Some(CheckStatus::Running)
    } else {
        Some(CheckStatus::Passed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Requested,
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
}

impl Verdict {
    fn parse(state: &str) -> Option<Self> {
        match state {
            "APPROVED" => Some(Verdict::Approved),
            "CHANGES_REQUESTED" => Some(Verdict::ChangesRequested),
            "COMMENTED" => Some(Verdict::Commented),
            "DISMISSED" => Some(Verdict::Dismissed),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Verdict::Requested => "review requested",
            Verdict::Approved => "approved",
            Verdict::ChangesRequested => "requested changes",
            Verdict::Commented => "commented",
            Verdict::Dismissed => "dismissed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer {
    pub login: String,
    pub verdict: Verdict,
}

/// Whether the branch can go in, from GitHub's merge state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeability {
    Ready,
    Conflicts,
    Blocked,
    Behind,
    Unstable,
    Unknown,
}

impl Mergeability {
    fn parse(merge_state: &str, mergeable: &str) -> Self {
        match merge_state {
            "CLEAN" | "HAS_HOOKS" => Mergeability::Ready,
            "DIRTY" => Mergeability::Conflicts,
            "BLOCKED" => Mergeability::Blocked,
            "BEHIND" => Mergeability::Behind,
            "UNSTABLE" => Mergeability::Unstable,
            _ if mergeable == "CONFLICTING" => Mergeability::Conflicts,
            _ => Mergeability::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Opened,
    Commit { sha: String, headline: String },
    Comment { body: String },
    Review { verdict: Verdict, body: String },
    Inline { path: String, body: String },
    Merged,
    Closed,
}

/// Something that happened on the pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at: String,
    pub actor: String,
    pub kind: EventKind,
}

/// A pull request or issue read in full. An issue leaves the branch, size,
/// reviewer, check and merge fields empty.
#[derive(Debug, Clone)]
pub struct Detail {
    pub pr: PrRef,
    pub kind: Kind,
    /// Who an issue is assigned to.
    pub assignees: Vec<String>,
    pub title: String,
    pub body: String,
    pub state: State,
    pub author: String,
    pub updated: String,
    pub base: String,
    pub head: String,
    /// The head commit; the diff is cached against it.
    pub head_sha: String,
    pub additions: u64,
    pub deletions: u64,
    pub labels: Vec<String>,
    pub reviewers: Vec<Reviewer>,
    pub checks: Vec<Check>,
    pub mergeability: Mergeability,
    /// The merge methods the repository allows, in the order offered.
    pub methods: Vec<MergeMethod>,
    /// Whether you may merge, close and relabel (write access or above).
    pub can_write: bool,
    /// Signed links to the description's uploads, by upload id, taken from the
    /// rendered body. They expire within minutes, so they are never cached.
    pub media: std::collections::HashMap<String, String>,
    pub timeline: Vec<Event>,
}

/// Something done to a pull request or an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Merge(MergeMethod),
    Close,
    /// Closes an issue, saying why.
    CloseAs(CloseReason),
    Reopen,
    Ready,
    Draft,
    Approve,
    Comment(String),
}

impl Action {
    fn args(&self, pr: &PrRef, kind: Kind) -> Vec<String> {
        let number = pr.number.to_string();
        let (verb, extra): (&str, Vec<String>) = match self {
            Action::Merge(method) => ("merge", vec![method.flag().to_owned()]),
            Action::Close => ("close", Vec::new()),
            Action::CloseAs(reason) => (
                "close",
                vec!["--reason".to_owned(), reason.label().to_owned()],
            ),
            Action::Reopen => ("reopen", Vec::new()),
            Action::Ready => ("ready", Vec::new()),
            Action::Draft => ("ready", vec!["--undo".to_owned()]),
            Action::Approve => ("review", vec!["--approve".to_owned()]),
            Action::Comment(body) => ("comment", vec!["--body".to_owned(), body.clone()]),
        };
        let noun = match kind {
            Kind::Pull => "pr",
            Kind::Issue => "issue",
        };
        let mut args = vec![
            noun.to_owned(),
            verb.to_owned(),
            number,
            "-R".to_owned(),
            pr.repo.clone(),
        ];
        args.extend(extra);
        args
    }

    /// What the footer says once it's done.
    pub fn done(&self, pr: &PrRef) -> String {
        let label = format!("#{}", pr.number);
        match self {
            Action::Merge(method) => format!("{label} merged ({})", method.label().to_lowercase()),
            Action::Close => format!("{label} closed"),
            Action::CloseAs(reason) => format!("{label} closed as {}", reason.label()),
            Action::Reopen => format!("{label} reopened"),
            Action::Ready => format!("{label} is ready for review"),
            Action::Draft => format!("{label} is a draft again"),
            Action::Approve => format!("Approved {label}"),
            Action::Comment(_) => format!("Commented on {label}"),
        }
    }
}

fn gh(args: &[&str]) -> Result<String, PrFailed> {
    if !*GH {
        return Err(PrFailed::NoGh);
    }
    let output = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| PrFailed::Gh {
            command: args.join(" "),
            cause: e.to_string(),
        })?;
    if !output.status.success() {
        let cause = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(PrFailed::Gh {
            command: args.join(" "),
            cause: if cause.is_empty() {
                format!("exited with {}", output.status)
            } else {
                cause
            },
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn gh_json(args: &[&str]) -> Result<Value, PrFailed> {
    let text = gh(args)?;
    serde_json::from_str(&text).map_err(|e| PrFailed::Unreadable {
        command: args.join(" "),
        cause: e.to_string(),
    })
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// The GitHub repository (`owner/name`) of the current folder, read from its
/// remotes without asking GitHub: `upstream` first, so a fork lists its parent.
pub fn current_repo() -> Option<String> {
    ["upstream", "origin"]
        .into_iter()
        .find_map(|remote| github_repo(&git(&["remote", "get-url", remote])?))
}

/// The branch checked out here, if any.
pub fn current_branch() -> Option<String> {
    git(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|branch| branch != "HEAD")
}

/// `owner/name` from a GitHub remote URL, over SSH or HTTPS.
fn github_repo(url: &str) -> Option<String> {
    let rest = [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
        "git://github.com/",
    ]
    .iter()
    .find_map(|prefix| url.strip_prefix(prefix))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => {
            Some(format!("{owner}/{name}"))
        }
        _ => None,
    }
}

/// What the list shows: pull requests or issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Pull,
    Issue,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Pull => "Pull requests",
            Kind::Issue => "Issues",
        }
    }

    fn qualifier(self) -> &'static str {
        match self {
            Kind::Pull => "is:pr",
            Kind::Issue => "is:issue",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Kind::Pull => "pr",
            Kind::Issue => "issue",
        }
    }
}

/// One search the list is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Branch { repo: String, branch: String },
    Repo(String),
    Authored(Kind),
    Reviewing,
    Owned(Kind),
}

impl Source {
    pub fn kind(&self) -> Kind {
        match self {
            Source::Branch { .. } | Source::Repo(_) | Source::Reviewing => Kind::Pull,
            Source::Authored(kind) | Source::Owned(kind) => *kind,
        }
    }

    fn query(&self, status: Status) -> Result<String, PrFailed> {
        let filter = match (self, status) {
            (Source::Branch { repo, branch }, _) => format!("repo:{repo} head:{branch}"),
            (Source::Repo(repo), _) => format!("repo:{repo}"),
            (Source::Authored(_), _) => "author:@me".to_owned(),
            // A request leaves once it's answered, so done ones are those you reviewed.
            (Source::Reviewing, Status::Open) => "review-requested:@me".to_owned(),
            (Source::Reviewing, Status::Done) => "reviewed-by:@me".to_owned(),
            (Source::Owned(_), _) => format!("user:{}", viewer()?),
        };
        let state = match (status, self.kind()) {
            (Status::Open, _) => "is:open",
            (Status::Done, Kind::Pull) => "is:merged",
            (Status::Done, Kind::Issue) => "is:closed",
        };
        Ok(format!(
            "{} {state} archived:false {filter} sort:updated-desc",
            self.kind().qualifier()
        ))
    }

    fn cache_key(&self, status: Status) -> String {
        let key = match self {
            Source::Branch { repo, branch } => format!("list-branch-{repo}-{branch}"),
            Source::Repo(repo) => format!("list-repo-{repo}"),
            Source::Authored(kind) => format!("list-authored-{}", kind.key()),
            Source::Reviewing => "list-reviewing".to_owned(),
            Source::Owned(kind) => format!("list-owned-{}", kind.key()),
        };
        match status {
            Status::Open => key,
            Status::Done => format!("{key}-done"),
        }
    }
}

/// The searches behind each tab. Pull requests: this branch's (a default
/// branch has none of its own), this repository's, yours, those waiting on
/// your review, and those in repositories you own. Issues: yours, and those in
/// repositories you own.
pub fn sources(here: Option<&str>, branch: Option<&str>) -> Vec<Source> {
    let mut out = Vec::new();
    if let Some(repo) = here {
        if let Some(branch) = branch.filter(|b| !matches!(*b, "main" | "master")) {
            out.push(Source::Branch {
                repo: repo.to_owned(),
                branch: branch.to_owned(),
            });
        }
        out.push(Source::Repo(repo.to_owned()));
    }
    out.extend([
        Source::Authored(Kind::Pull),
        Source::Reviewing,
        Source::Owned(Kind::Pull),
        Source::Authored(Kind::Issue),
        Source::Owned(Kind::Issue),
    ]);
    out
}

/// How a row concerns you, which picks its color in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Relation {
    /// Yours, in your own repository.
    Yours,
    /// Yours, in someone else's repository.
    Outgoing,
    /// Someone else's, in your repository.
    Incoming,
    /// Someone else's, elsewhere: this repository's, or waiting on your review.
    Other,
}

impl Relation {
    pub const ALL: [Relation; 4] = [
        Relation::Yours,
        Relation::Outgoing,
        Relation::Incoming,
        Relation::Other,
    ];

    pub fn of(entry: &Entry, me: &str) -> Self {
        let owner = entry.pr.repo.split('/').next().unwrap_or_default();
        match (
            entry.author.eq_ignore_ascii_case(me),
            owner.eq_ignore_ascii_case(me),
        ) {
            (true, true) => Relation::Yours,
            (true, false) => Relation::Outgoing,
            (false, true) => Relation::Incoming,
            (false, false) => Relation::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Relation::Yours => "yours",
            Relation::Outgoing => "to others",
            Relation::Incoming => "from others",
            Relation::Other => "other",
        }
    }
}

/// Your GitHub login: from gh's own config, which needs no request, or else
/// asked. Kept, so the list can color its rows at once next time.
pub fn viewer() -> Result<String, PrFailed> {
    let login = match gh(&["config", "get", "user", "-h", "github.com"]) {
        Ok(login) if !login.is_empty() => login,
        _ => gh(&["api", "user", "-q", ".login"])?,
    };
    store("viewer", &login);
    Ok(login)
}

/// The login `viewer` found last time.
pub fn cached_viewer() -> Option<String> {
    load("viewer").filter(|login| !login.is_empty())
}

/// One GraphQL search: much faster than `gh search prs`, which goes through REST.
const SEARCH: &str = "query($q: String!, $after: String) { search(query: $q, type: ISSUE, first: 100, after: $after) { pageInfo { hasNextPage endCursor } nodes { ... on PullRequest { number title isDraft updatedAt mergedAt author { login } repository { nameWithOwner } } ... on Issue { number title updatedAt closedAt stateReason author { login } repository { nameWithOwner } } } } }";

/// How many pages of 100 a done search reads. Open searches read one.
const DONE_PAGES: usize = 3;

/// The pull requests or issues one search finds, newest first: open ones, or
/// merged and closed ones. The answer is cached, so the list shows at once
/// next time while it's asked again.
pub fn search(source: &Source, status: Status) -> Result<Vec<Entry>, PrFailed> {
    if !*GH {
        return Err(PrFailed::NoGh);
    }
    let query = source.query(status)?;
    let pages = match status {
        Status::Open => 1,
        Status::Done => DONE_PAGES,
    };
    let mut nodes = Vec::new();
    let mut after: Option<String> = None;
    let mut more = false;
    for _ in 0..pages {
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={SEARCH}"),
            "-f".to_owned(),
            format!("q={query}"),
        ];
        if let Some(cursor) = &after {
            args.extend(["-f".to_owned(), format!("after={cursor}")]);
        }
        let json = gh_json(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        let search = json.pointer("/data/search").cloned().unwrap_or(Value::Null);
        nodes.extend(array(&search, "nodes").iter().cloned());
        more = search
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        after = search
            .pointer("/pageInfo/endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if !more || after.is_none() {
            break;
        }
    }
    let nodes = match status {
        Status::Done if more => complete_since(nodes),
        _ => nodes,
    };
    let nodes = Value::Array(nodes);
    store(&source.cache_key(status), &nodes.to_string());
    Ok(parse_search(&nodes))
}

/// A done search cut off, by most recently updated, keeps only the rows that
/// ended after the last row read was updated. A row can't end after its last
/// update, so every row that ended after then was read, and the list has no
/// gaps by end time; an older row could have a newer one missing above it.
fn complete_since(nodes: Vec<Value>) -> Vec<Value> {
    let Some(cutoff) = nodes
        .iter()
        .filter_map(|node| seconds(node.get("updatedAt")?.as_str()?))
        .min()
    else {
        return nodes;
    };
    nodes
        .into_iter()
        .filter(|node| {
            ended(node)
                .and_then(|ended| seconds(&ended.at))
                .is_none_or(|at| at >= cutoff)
        })
        .collect()
}

/// What `search` found last time, if it has run before.
pub fn cached_search(source: &Source, status: Status) -> Option<Vec<Entry>> {
    let nodes: Value = serde_json::from_str(&load(&source.cache_key(status))?).ok()?;
    Some(parse_search(&nodes))
}

/// One list from several searches: each row once, newest first. When two
/// searches cached the same row, the fresher copy wins.
pub fn merge(lists: impl IntoIterator<Item = Vec<Entry>>) -> Vec<Entry> {
    let mut by_ref: std::collections::HashMap<PrRef, Entry> = std::collections::HashMap::new();
    for entry in lists.into_iter().flatten() {
        let fresher = match by_ref.get(&entry.pr) {
            Some(kept) => seconds(&entry.updated) > seconds(&kept.updated),
            None => true,
        };
        if fresher {
            by_ref.insert(entry.pr.clone(), entry);
        }
    }
    let mut out: Vec<Entry> = by_ref.into_values().collect();
    // Rows updated at the same second keep a fixed order, so the picked row
    // doesn't jump between redraws.
    // Done rows go by when they ended, open ones by when they last changed.
    out.sort_by(|a, b| {
        seconds(b.when())
            .cmp(&seconds(a.when()))
            .then_with(|| a.pr.repo.cmp(&b.pr.repo))
            .then_with(|| b.pr.number.cmp(&a.pr.number))
    });
    out
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn login(value: &Value, key: &str) -> String {
    value
        .get(key)
        .map(|author| text(author, "login"))
        .filter(|login| !login.is_empty())
        .unwrap_or_else(|| "ghost".to_owned())
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn entry(item: &Value, repo: String) -> Entry {
    Entry {
        pr: PrRef {
            repo,
            number: number(item, "number"),
        },
        title: text(item, "title"),
        author: login(item, "author"),
        updated: text(item, "updatedAt"),
        draft: item
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ended: ended(item),
    }
}

/// A merged pull request's or closed issue's ending, read from the search
/// answer; `None` for an open one, or an answer cached before leaf asked.
fn ended(item: &Value) -> Option<Ended> {
    let at = |key| {
        item.get(key)
            .and_then(Value::as_str)
            .filter(|at| !at.is_empty())
            .map(str::to_owned)
    };
    if let Some(at) = at("mergedAt") {
        return Some(Ended {
            how: Ending::Merged,
            at,
        });
    }
    let how = match item.get("stateReason").and_then(Value::as_str) {
        Some("NOT_PLANNED") => Ending::NotPlanned,
        _ => Ending::Completed,
    };
    at("closedAt").map(|at| Ended { how, at })
}

fn parse_search(json: &Value) -> Vec<Entry> {
    json.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let repo = item.get("repository").map(|r| text(r, "nameWithOwner"))?;
                    (!repo.is_empty()).then(|| entry(item, repo))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Where answers from GitHub are kept between runs.
fn cache_path(key: &str) -> Option<std::path::PathBuf> {
    cache_file(&format!("{key}.json"))
}

/// A file in leaf's pull request cache, `$XDG_CACHE_HOME/leaf/pr`.
pub(crate) fn cache_file(key: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".cache"))
        })?;
    let name: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(base.join("leaf").join("pr").join(name))
}

fn load(key: &str) -> Option<String> {
    std::fs::read_to_string(cache_path(key)?).ok()
}

/// Saves an answer for next time. A cache that can't be written only means
/// the next start waits for GitHub, so failures are let go.
fn store(key: &str, text: &str) {
    let Some(path) = cache_path(key) else { return };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Written aside, then moved, so a reader never sees half a file.
    let partial = path.with_extension(format!("{}.part", std::process::id()));
    if std::fs::write(&partial, text).is_ok() && std::fs::rename(&partial, &path).is_err() {
        let _ = std::fs::remove_file(&partial);
    }
}

const VIEW_FIELDS: &str = "number,title,body,state,isDraft,author,createdAt,updatedAt,mergedAt,closedAt,baseRefName,headRefName,headRefOid,additions,deletions,labels,reviewRequests,latestReviews,reviews,comments,commits,statusCheckRollup,mergeable,mergeStateStatus";

fn view_key(pr: &PrRef) -> String {
    format!("view-{}-{}", pr.repo, pr.number)
}

/// Reads a pull request or an issue in full.
pub fn view(pr: &PrRef, kind: Kind) -> Result<Detail, PrFailed> {
    if !*GH {
        return Err(PrFailed::NoGh);
    }
    match kind {
        Kind::Pull => view_pull(pr),
        Kind::Issue => view_issue(pr),
    }
}

/// The page or issue as `view` read it last time, if it has been read before.
pub fn cached_view(pr: &PrRef, kind: Kind) -> Option<Detail> {
    match kind {
        Kind::Pull => {
            let saved: Value = serde_json::from_str(&load(&view_key(pr))?).ok()?;
            Some(parse_saved(pr.clone(), &saved))
        }
        Kind::Issue => {
            let page: Value = serde_json::from_str(&load(&issue_key(pr))?).ok()?;
            Some(parse_issue(pr.clone(), &page))
        }
    }
}

const ISSUE_FIELDS: &str = "number,title,body,state,stateReason,author,createdAt,updatedAt,closedAt,labels,assignees,comments";

fn issue_key(pr: &PrRef) -> String {
    format!("issue-{}-{}", pr.repo, pr.number)
}

/// Reads an issue: its page and comments, and the rendered body for its uploads.
fn view_issue(issue: &PrRef) -> Result<Detail, PrFailed> {
    let rendered = {
        let path = format!("repos/{}/issues/{}", issue.repo, issue.number);
        std::thread::spawn(move || {
            gh(&[
                "api",
                &path,
                "-H",
                "Accept: application/vnd.github.full+json",
                "--jq",
                ".body_html",
            ])
        })
    };
    let number = issue.number.to_string();
    let page = gh_json(&[
        "issue",
        "view",
        &number,
        "-R",
        &issue.repo,
        "--json",
        ISSUE_FIELDS,
    ])?;
    store(&issue_key(issue), &page.to_string());
    let mut detail = parse_issue(issue.clone(), &page);
    if let Some(html) = rendered.join().ok().and_then(Result::ok) {
        detail.media = crate::prmedia::signed_links(&html);
    }
    Ok(detail)
}

fn parse_issue(issue: PrRef, page: &Value) -> Detail {
    let state = match (
        text(page, "state").as_str(),
        text(page, "stateReason").as_str(),
    ) {
        ("CLOSED", "NOT_PLANNED") => State::NotPlanned,
        ("CLOSED", _) => State::Completed,
        _ => State::Open,
    };
    let names = |key: &str, field: &str| -> Vec<String> {
        array(page, key)
            .iter()
            .map(|item| text(item, field))
            .filter(|name| !name.is_empty())
            .collect()
    };
    let author = login(page, "author");
    let created = text(page, "createdAt");
    Detail {
        kind: Kind::Issue,
        assignees: names("assignees", "login"),
        title: text(page, "title"),
        body: text(page, "body"),
        state,
        updated: text(page, "updatedAt"),
        base: String::new(),
        head: String::new(),
        head_sha: String::new(),
        additions: 0,
        deletions: 0,
        labels: names("labels", "name"),
        reviewers: Vec::new(),
        checks: Vec::new(),
        mergeability: Mergeability::Unknown,
        methods: Vec::new(),
        // GitHub decides who may close or reopen; leaf lets it refuse.
        can_write: true,
        media: std::collections::HashMap::new(),
        timeline: timeline(page, &Value::Null, &author, &created, state),
        author,
        pr: issue,
    }
}

/// Reads a pull request in full: its page, the repository's merge settings,
/// and the comments left on lines of the diff, all at once.
fn view_pull(pr: &PrRef) -> Result<Detail, PrFailed> {
    let settings = {
        let repo = pr.repo.clone();
        std::thread::spawn(move || {
            gh_json(&[
                "repo",
                "view",
                &repo,
                "--json",
                "squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed,viewerPermission",
            ])
        })
    };
    let inline = {
        let path = format!(
            "repos/{}/pulls/{}/comments?per_page=100",
            pr.repo, pr.number
        );
        std::thread::spawn(move || gh_json(&["api", &path]))
    };
    // The rendered body carries signed links to uploads, which work where
    // github.com wants a browser session (repositories enforcing SAML SSO).
    let rendered = {
        let path = format!("repos/{}/pulls/{}", pr.repo, pr.number);
        std::thread::spawn(move || {
            gh(&[
                "api",
                &path,
                "-H",
                "Accept: application/vnd.github.full+json",
                "--jq",
                ".body_html",
            ])
        })
    };
    let number = pr.number.to_string();
    let page = gh_json(&["pr", "view", &number, "-R", &pr.repo, "--json", VIEW_FIELDS])?;
    // Without the settings, offer every method and let GitHub refuse.
    let settings = settings
        .join()
        .ok()
        .and_then(Result::ok)
        .unwrap_or(Value::Null);
    let inline = inline
        .join()
        .ok()
        .and_then(Result::ok)
        .unwrap_or(Value::Null);
    let saved = serde_json::json!({ "page": page, "settings": settings, "inline": inline });
    store(&view_key(pr), &saved.to_string());
    let mut detail = parse_saved(pr.clone(), &saved);
    // Without the rendered body, uploads are signed through github.com instead.
    if let Some(html) = rendered.join().ok().and_then(Result::ok) {
        detail.media = crate::prmedia::signed_links(&html);
    }
    Ok(detail)
}

fn patch_key(pr: &PrRef, head: &str) -> String {
    format!("patch-{}-{}-{head}", pr.repo, pr.number)
}

/// The pull request's patch at commit `head`, if it was fetched before.
/// A push moves the head, so a stale patch is never shown.
pub fn cached_patch(pr: &PrRef, head: &str) -> Option<String> {
    (!head.is_empty())
        .then(|| load(&patch_key(pr, head)))
        .flatten()
}

/// Fetches the pull request's patch, keeping it against `head` when known.
pub fn fetch_patch(pr: &PrRef, head: &str) -> Result<String, String> {
    let fetched = crate::github::fetch(&pr.url()).map_err(|e| e.to_string())?;
    if !head.is_empty() {
        store(&patch_key(pr, head), &fetched.patch);
    }
    Ok(fetched.patch)
}

fn parse_saved(pr: PrRef, saved: &Value) -> Detail {
    let settings = saved.get("settings").filter(|s| !s.is_null());
    parse_detail(
        pr,
        saved.get("page").unwrap_or(&Value::Null),
        settings,
        saved.get("inline").unwrap_or(&Value::Null),
    )
}

fn parse_detail(pr: PrRef, page: &Value, settings: Option<&Value>, inline: &Value) -> Detail {
    let state = match (text(page, "state").as_str(), page.get("isDraft")) {
        ("MERGED", _) => State::Merged,
        ("CLOSED", _) => State::Closed,
        (_, Some(Value::Bool(true))) => State::Draft,
        _ => State::Open,
    };
    let names = |key: &str, field: &str| -> Vec<String> {
        array(page, key)
            .iter()
            .map(|item| text(item, field))
            .filter(|name| !name.is_empty())
            .collect()
    };

    let mut reviewers: Vec<Reviewer> = Vec::new();
    for review in array(page, "latestReviews") {
        if let Some(verdict) = Verdict::parse(&text(review, "state")) {
            reviewers.push(Reviewer {
                login: login(review, "author"),
                verdict,
            });
        }
    }
    for request in array(page, "reviewRequests") {
        // A team asks by name, a person by login.
        let name = Some(text(request, "login"))
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| text(request, "name"));
        if !name.is_empty() && !reviewers.iter().any(|r| r.login == name) {
            reviewers.push(Reviewer {
                login: name,
                verdict: Verdict::Requested,
            });
        }
    }

    let methods = match settings {
        Some(settings) => [
            ("squashMergeAllowed", MergeMethod::Squash),
            ("mergeCommitAllowed", MergeMethod::Merge),
            ("rebaseMergeAllowed", MergeMethod::Rebase),
        ]
        .into_iter()
        .filter(|(key, _)| settings.get(key).and_then(Value::as_bool).unwrap_or(false))
        .map(|(_, method)| method)
        .collect(),
        None => vec![MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase],
    };
    let can_write = settings
        .map(|s| text(s, "viewerPermission"))
        .is_none_or(|p| matches!(p.as_str(), "ADMIN" | "MAINTAIN" | "WRITE"));

    let author = login(page, "author");
    let created = text(page, "createdAt");
    Detail {
        kind: Kind::Pull,
        assignees: Vec::new(),
        title: text(page, "title"),
        body: text(page, "body"),
        state,
        updated: text(page, "updatedAt"),
        base: text(page, "baseRefName"),
        head: text(page, "headRefName"),
        head_sha: text(page, "headRefOid"),
        additions: number(page, "additions"),
        deletions: number(page, "deletions"),
        labels: names("labels", "name"),
        reviewers,
        checks: array(page, "statusCheckRollup")
            .iter()
            .map(parse_check)
            .collect(),
        mergeability: Mergeability::parse(
            &text(page, "mergeStateStatus"),
            &text(page, "mergeable"),
        ),
        methods,
        can_write,
        media: std::collections::HashMap::new(),
        timeline: timeline(page, inline, &author, &created, state),
        author,
        pr,
    }
}

fn parse_check(check: &Value) -> Check {
    let (name, url) = match text(check, "__typename").as_str() {
        "StatusContext" => (text(check, "context"), text(check, "targetUrl")),
        _ => {
            let workflow = text(check, "workflowName");
            let name = text(check, "name");
            let name = if workflow.is_empty() || workflow == name {
                name
            } else {
                format!("{workflow} / {name}")
            };
            (name, text(check, "detailsUrl"))
        }
    };
    let status = match text(check, "__typename").as_str() {
        "StatusContext" => match text(check, "state").as_str() {
            "SUCCESS" => CheckStatus::Passed,
            "FAILURE" | "ERROR" => CheckStatus::Failed,
            _ => CheckStatus::Running,
        },
        _ if text(check, "status") != "COMPLETED" => CheckStatus::Running,
        _ => match text(check, "conclusion").as_str() {
            "SUCCESS" | "NEUTRAL" => CheckStatus::Passed,
            "SKIPPED" => CheckStatus::Skipped,
            "ACTION_REQUIRED" => CheckStatus::Running,
            _ => CheckStatus::Failed,
        },
    };
    Check {
        name,
        status,
        url: (!url.is_empty()).then_some(url),
    }
}

fn timeline(page: &Value, inline: &Value, author: &str, created: &str, state: State) -> Vec<Event> {
    let mut events = vec![Event {
        at: created.to_owned(),
        actor: author.to_owned(),
        kind: EventKind::Opened,
    }];
    for commit in array(page, "commits") {
        let actor = array(commit, "authors")
            .first()
            .map(|a| {
                Some(text(a, "login"))
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| text(a, "name"))
            })
            .unwrap_or_default();
        let sha = text(commit, "oid");
        events.push(Event {
            at: text(commit, "committedDate"),
            actor,
            kind: EventKind::Commit {
                sha: sha.chars().take(7).collect(),
                headline: text(commit, "messageHeadline"),
            },
        });
    }
    for comment in array(page, "comments") {
        events.push(Event {
            at: text(comment, "createdAt"),
            actor: login(comment, "author"),
            kind: EventKind::Comment {
                body: text(comment, "body"),
            },
        });
    }
    for review in array(page, "reviews") {
        let Some(verdict) = Verdict::parse(&text(review, "state")) else {
            continue;
        };
        let body = text(review, "body");
        // A bare "commented" review is only the wrapper of line comments shown on their own.
        if verdict == Verdict::Commented && visible(&body).is_empty() {
            continue;
        }
        events.push(Event {
            at: text(review, "submittedAt"),
            actor: login(review, "author"),
            kind: EventKind::Review { verdict, body },
        });
    }
    for comment in inline.as_array().map(Vec::as_slice).unwrap_or_default() {
        events.push(Event {
            at: text(comment, "created_at"),
            actor: login(comment, "user"),
            kind: EventKind::Inline {
                path: text(comment, "path"),
                body: text(comment, "body"),
            },
        });
    }
    match state {
        State::Merged => events.push(Event {
            at: text(page, "mergedAt"),
            actor: String::new(),
            kind: EventKind::Merged,
        }),
        State::Closed | State::Completed | State::NotPlanned => events.push(Event {
            at: text(page, "closedAt"),
            actor: String::new(),
            kind: EventKind::Closed,
        }),
        State::Open | State::Draft => {}
    }
    // Oldest first, like a conversation; the sort is stable for equal times.
    events.sort_by_key(|event| seconds(&event.at));
    events
}

/// The body without the HTML comments bots hide state in.
pub fn visible(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// Does `action` to the pull request or issue and says what was done.
pub fn perform(pr: &PrRef, kind: Kind, action: &Action) -> Result<String, PrFailed> {
    let args = action.args(pr, kind);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    gh(&refs)?;
    Ok(action.done(pr))
}

/// Opens the pull request's page in the browser.
pub fn open_in_browser(pr: &PrRef) -> Result<(), PrFailed> {
    let number = pr.number.to_string();
    gh(&["pr", "view", &number, "-R", &pr.repo, "--web"]).map(|_| ())
}

/// Opens an issue's page in the browser.
pub fn open_issue(issue: &PrRef) -> Result<(), PrFailed> {
    let number = issue.number.to_string();
    gh(&["issue", "view", &number, "-R", &issue.repo, "--web"]).map(|_| ())
}

/// Seconds since 1970 for an ISO 8601 UTC time such as `2026-11-12T04:29:54Z`.
pub fn seconds(iso: &str) -> Option<i64> {
    let bytes = iso.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let field = |range: std::ops::Range<usize>| iso.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    // Days from the civil date, after Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// How long ago `iso` was, shortly: `now`, `5m ago`, `3h ago`, `2d ago`, or the date.
pub fn ago(iso: &str, now: i64) -> String {
    let Some(then) = seconds(iso) else {
        return String::new();
    };
    let elapsed = (now - then).max(0);
    match elapsed {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m ago", elapsed / 60),
        3_600..86_400 => format!("{}h ago", elapsed / 3_600),
        86_400..2_592_000 => format!("{}d ago", elapsed / 86_400),
        _ => iso.get(..10).unwrap_or(iso).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets_parse_from_numbers_names_and_links() {
        let here = Some("o/r");
        let pr = |repo: &str, number| PrRef {
            repo: repo.to_owned(),
            number,
        };
        assert_eq!(PrRef::parse("12", here).unwrap(), pr("o/r", 12));
        assert_eq!(PrRef::parse("#12", here).unwrap(), pr("o/r", 12));
        assert_eq!(PrRef::parse("a/b#3", here).unwrap(), pr("a/b", 3));
        assert_eq!(
            PrRef::parse("https://github.com/a/b/pull/9/files", None).unwrap(),
            pr("a/b", 9)
        );
        assert!(matches!(
            PrRef::parse("12", None),
            Err(PrFailed::NoRepo { number: 12 })
        ));
        assert!(matches!(
            PrRef::parse("a#3", here),
            Err(PrFailed::BadTarget { .. })
        ));
        assert!(matches!(
            PrRef::parse("main", here),
            Err(PrFailed::BadTarget { .. })
        ));
    }

    #[test]
    fn remotes_name_their_github_repository() {
        for url in [
            "git@github.com:o/r.git",
            "https://github.com/o/r",
            "https://github.com/o/r.git/",
            "ssh://git@github.com/o/r.git",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("o/r"), "{url}");
        }
        assert_eq!(github_repo("git@gitlab.com:o/r.git"), None);
        assert_eq!(github_repo("https://github.com/o"), None);
    }

    #[test]
    fn the_list_leads_with_this_branch_then_this_repository() {
        assert_eq!(
            sources(Some("o/r"), Some("fix/q")),
            [
                Source::Branch {
                    repo: "o/r".into(),
                    branch: "fix/q".into()
                },
                Source::Repo("o/r".into()),
                Source::Authored(Kind::Pull),
                Source::Reviewing,
                Source::Owned(Kind::Pull),
                Source::Authored(Kind::Issue),
                Source::Owned(Kind::Issue),
            ]
        );
        assert_eq!(
            sources(Some("o/r"), Some("main"))[0],
            Source::Repo("o/r".into())
        );
        assert_eq!(
            sources(None, Some("fix/q"))[0],
            Source::Authored(Kind::Pull)
        );
        assert_eq!(
            Source::Authored(Kind::Issue).query(Status::Open).unwrap(),
            "is:issue is:open archived:false author:@me sort:updated-desc"
        );
        assert_eq!(
            Source::Branch {
                repo: "o/r".into(),
                branch: "fix/q".into()
            }
            .query(Status::Open)
            .unwrap(),
            "is:pr is:open archived:false repo:o/r head:fix/q sort:updated-desc"
        );
    }

    #[test]
    fn done_searches_ask_for_merged_and_closed_rows() {
        assert_eq!(
            Source::Authored(Kind::Pull).query(Status::Done).unwrap(),
            "is:pr is:merged archived:false author:@me sort:updated-desc"
        );
        assert_eq!(
            Source::Authored(Kind::Issue).query(Status::Done).unwrap(),
            "is:issue is:closed archived:false author:@me sort:updated-desc"
        );
        assert_eq!(
            Source::Reviewing.query(Status::Done).unwrap(),
            "is:pr is:merged archived:false reviewed-by:@me sort:updated-desc"
        );
        assert_ne!(
            Source::Reviewing.cache_key(Status::Open),
            Source::Reviewing.cache_key(Status::Done)
        );
    }

    #[test]
    fn a_cut_off_done_search_keeps_only_rows_with_nothing_missing_above() {
        let node = |number: u64, updated: &str, merged: &str| serde_json::json!({"number": number, "updatedAt": updated, "mergedAt": merged});
        let kept = complete_since(vec![
            node(1, "2026-11-10T00:00:00Z", "2026-11-09T00:00:00Z"),
            // Updated lately, merged long ago: a row merged between the two
            // could have been cut off, so this one goes.
            node(2, "2026-11-08T00:00:00Z", "2026-01-01T00:00:00Z"),
            node(3, "2026-11-05T00:00:00Z", "2026-11-05T00:00:00Z"),
        ]);
        let numbers: Vec<u64> = kept
            .iter()
            .filter_map(|n| n.get("number")?.as_u64())
            .collect();
        assert_eq!(numbers, [1, 3]);
    }

    #[test]
    fn done_rows_carry_how_and_when_they_ended_and_sort_by_it() {
        let nodes = serde_json::json!([
            {"number": 1, "title": "a", "updatedAt": "2026-11-10T00:00:00Z",
             "mergedAt": "2026-11-01T00:00:00Z", "author": {"login": "me"},
             "repository": {"nameWithOwner": "o/r"}},
            {"number": 2, "title": "b", "updatedAt": "2026-11-02T00:00:00Z",
             "closedAt": "2026-11-02T00:00:00Z", "stateReason": "NOT_PLANNED",
             "author": {"login": "me"}, "repository": {"nameWithOwner": "o/r"}},
            {"number": 3, "title": "c", "updatedAt": "2026-11-03T00:00:00Z",
             "author": {"login": "me"}, "repository": {"nameWithOwner": "o/r"}}
        ]);
        let rows = parse_search(&nodes);
        assert_eq!(rows[0].ended.as_ref().map(|e| e.how), Some(Ending::Merged));
        assert_eq!(
            rows[1].ended.as_ref().map(|e| e.how),
            Some(Ending::NotPlanned)
        );
        assert_eq!(rows[2].ended, None);
        // #1 changed last but merged first, so it goes last.
        let order: Vec<u64> = merge([rows]).iter().map(|e| e.pr.number).collect();
        assert_eq!(order, [3, 2, 1]);
    }

    #[test]
    fn saved_answers_read_back_as_the_same_pull_request() {
        let pr = PrRef {
            repo: "o/r".into(),
            number: 3,
        };
        let saved =
            json!({ "page": {"title": "t", "state": "OPEN"}, "settings": null, "inline": null });
        let detail = parse_saved(pr, &saved);
        assert_eq!(detail.title, "t");
        // No settings: every method is offered and GitHub decides.
        assert_eq!(detail.methods.len(), 3);
    }

    #[test]
    fn times_count_back_from_now() {
        assert_eq!(seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
        let now = seconds("2026-11-12T04:30:00Z").unwrap();
        assert_eq!(ago("2026-11-12T04:29:54Z", now), "now");
        assert_eq!(ago("2026-11-12T04:20:00Z", now), "10m ago");
        assert_eq!(ago("2026-11-11T22:30:00Z", now), "6h ago");
        assert_eq!(ago("2026-11-09T04:30:00Z", now), "3d ago");
        assert_eq!(ago("2025-01-02T00:00:00Z", now), "2025-01-02");
        assert_eq!(ago("", now), "");
    }

    #[test]
    fn checks_add_up_like_t3_code_says_them() {
        let check = |status| Check {
            name: "ci".into(),
            status,
            url: None,
        };
        assert_eq!(describe_checks(&[]), "No checks");
        assert_eq!(
            describe_checks(&[check(CheckStatus::Passed), check(CheckStatus::Skipped)]),
            "All checks passed"
        );
        assert_eq!(
            describe_checks(&[check(CheckStatus::Passed), check(CheckStatus::Failed)]),
            "1 of 2 failing"
        );
        assert_eq!(
            describe_checks(&[check(CheckStatus::Running), check(CheckStatus::Failed)]),
            "1 of 2 running · 1 failed"
        );
        assert_eq!(
            checks_status(&[check(CheckStatus::Running), check(CheckStatus::Passed)]),
            Some(CheckStatus::Running)
        );
    }

    #[test]
    fn bot_state_in_html_comments_is_hidden() {
        assert_eq!(visible("<!-- x -->\n\n**Done**<!-- y\nz -->!"), "**Done**!");
        assert_eq!(visible("<!-- open"), "");
    }

    #[test]
    fn a_pull_request_reads_into_its_parts() {
        let page = json!({
            "number": 7, "title": "fix: quotes", "body": "Body", "state": "OPEN", "isDraft": false,
            "author": {"login": "marcos"}, "createdAt": "2026-11-11T22:00:00Z",
            "updatedAt": "2026-11-12T04:00:00Z", "baseRefName": "main", "headRefName": "fix/q",
            "additions": 57, "deletions": 4, "changedFiles": 2,
            "labels": [{"name": "size:M"}], "assignees": [],
            "reviewRequests": [{"login": "bob"}, {"name": "core"}],
            "latestReviews": [{"author": {"login": "ann"}, "state": "APPROVED"}],
            "reviews": [
                {"author": {"login": "ann"}, "state": "APPROVED", "body": "", "submittedAt": "2026-11-12T01:00:00Z"},
                {"author": {"login": "bot"}, "state": "COMMENTED", "body": "<!-- hidden -->", "submittedAt": "2026-11-12T01:00:00Z"}
            ],
            "comments": [{"author": {"login": "cy"}, "body": "nice", "createdAt": "2026-11-11T23:00:00Z"}],
            "commits": [{"oid": "a93cd2543365", "messageHeadline": "fix: quotes", "committedDate": "2026-11-11T22:30:00Z", "authors": [{"login": "marcos"}]}],
            "statusCheckRollup": [
                {"__typename": "StatusContext", "context": "CodeRabbit", "state": "SUCCESS", "targetUrl": ""},
                {"__typename": "CheckRun", "name": "test", "workflowName": "CI", "status": "IN_PROGRESS", "conclusion": "", "detailsUrl": "https://x"}
            ],
            "mergeable": "MERGEABLE", "mergeStateStatus": "BLOCKED"
        });
        let settings = json!({"squashMergeAllowed": true, "mergeCommitAllowed": false, "rebaseMergeAllowed": true, "viewerPermission": "READ"});
        let inline = json!([{"user": {"login": "dee"}, "path": "src/find.rs", "body": "why?", "created_at": "2026-11-12T02:00:00Z"}]);
        let pr = PrRef {
            repo: "o/r".into(),
            number: 7,
        };
        let detail = parse_detail(pr, &page, Some(&settings), &inline);
        assert_eq!(detail.state, State::Open);
        assert_eq!(detail.labels, ["size:M"]);
        assert_eq!(
            detail.reviewers,
            [
                Reviewer {
                    login: "ann".into(),
                    verdict: Verdict::Approved
                },
                Reviewer {
                    login: "bob".into(),
                    verdict: Verdict::Requested
                },
                Reviewer {
                    login: "core".into(),
                    verdict: Verdict::Requested
                },
            ]
        );
        assert_eq!(detail.checks[0].status, CheckStatus::Passed);
        assert_eq!(detail.checks[1].name, "CI / test");
        assert_eq!(detail.checks[1].status, CheckStatus::Running);
        assert_eq!(detail.mergeability, Mergeability::Blocked);
        assert_eq!(detail.methods, [MergeMethod::Squash, MergeMethod::Rebase]);
        assert!(!detail.can_write);
        let kinds: Vec<&str> = detail
            .timeline
            .iter()
            .map(|e| match e.kind {
                EventKind::Opened => "opened",
                EventKind::Commit { .. } => "commit",
                EventKind::Comment { .. } => "comment",
                EventKind::Review { .. } => "review",
                EventKind::Inline { .. } => "inline",
                EventKind::Merged => "merged",
                EventKind::Closed => "closed",
            })
            .collect();
        assert_eq!(kinds, ["opened", "commit", "comment", "review", "inline"]);
    }

    #[test]
    fn drafts_merged_and_closed_read_from_state() {
        let pr = PrRef {
            repo: "o/r".into(),
            number: 1,
        };
        let state = |page: Value| parse_detail(pr.clone(), &page, None, &Value::Null).state;
        assert_eq!(
            state(json!({"state": "OPEN", "isDraft": true})),
            State::Draft
        );
        assert_eq!(
            state(json!({"state": "MERGED", "isDraft": false})),
            State::Merged
        );
        assert_eq!(
            state(json!({"state": "CLOSED", "isDraft": true})),
            State::Closed
        );
    }

    #[test]
    fn each_pull_request_lists_once_newest_first() {
        let e = |number, updated: &str| Entry {
            pr: PrRef {
                repo: "o/r".into(),
                number,
            },
            title: String::new(),
            author: String::new(),
            updated: updated.into(),
            draft: false,
            ended: None,
        };
        let merged = merge([
            vec![e(1, "2026-01-01T00:00:00Z"), e(2, "2026-02-01T00:00:00Z")],
            vec![e(2, "2026-02-01T00:00:00Z"), e(3, "2026-03-01T00:00:00Z")],
        ]);
        let numbers: Vec<_> = merged.iter().map(|e| e.pr.number).collect();
        assert_eq!(numbers, [3, 2, 1]);
    }

    #[test]
    fn the_fresher_copy_of_a_row_wins() {
        let e = |title: &str, updated: &str| Entry {
            pr: PrRef {
                repo: "o/r".into(),
                number: 7,
            },
            title: title.into(),
            author: String::new(),
            updated: updated.into(),
            draft: false,
            ended: None,
        };
        let merged = merge([
            vec![e("old", "2026-01-01T00:00:00Z")],
            vec![e("new", "2026-02-01T00:00:00Z")],
            vec![e("older", "2025-12-01T00:00:00Z")],
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].title, "new");
    }

    #[test]
    fn rows_updated_together_keep_a_fixed_order() {
        let e = |repo: &str, number| Entry {
            pr: PrRef {
                repo: repo.into(),
                number,
            },
            title: String::new(),
            author: String::new(),
            updated: "2026-01-01T00:00:00Z".into(),
            draft: false,
            ended: None,
        };
        let rows = |lists: Vec<Vec<Entry>>| -> Vec<(String, u64)> {
            merge(lists)
                .into_iter()
                .map(|e| (e.pr.repo, e.pr.number))
                .collect()
        };
        let one = rows(vec![vec![e("b/r", 1), e("a/r", 2), e("a/r", 5)]]);
        let other = rows(vec![vec![e("a/r", 5)], vec![e("a/r", 2), e("b/r", 1)]]);
        assert_eq!(one, other);
        assert_eq!(one[0], ("a/r".to_owned(), 5));
    }

    #[test]
    fn rows_are_colored_by_whose_they_are_and_where() {
        let e = |repo: &str, author: &str| Entry {
            pr: PrRef {
                repo: repo.into(),
                number: 1,
            },
            title: String::new(),
            author: author.into(),
            updated: String::new(),
            draft: false,
            ended: None,
        };
        assert_eq!(Relation::of(&e("Me/leaf", "me"), "me"), Relation::Yours);
        assert_eq!(Relation::of(&e("o/r", "me"), "me"), Relation::Outgoing);
        assert_eq!(Relation::of(&e("me/leaf", "ann"), "me"), Relation::Incoming);
        assert_eq!(Relation::of(&e("o/r", "ann"), "me"), Relation::Other);
    }

    #[test]
    fn actions_become_gh_commands() {
        let pr = PrRef {
            repo: "o/r".into(),
            number: 7,
        };
        assert_eq!(
            Action::Merge(MergeMethod::Squash).args(&pr, Kind::Pull),
            ["pr", "merge", "7", "-R", "o/r", "--squash"]
        );
        assert_eq!(
            Action::Draft.args(&pr, Kind::Pull),
            ["pr", "ready", "7", "-R", "o/r", "--undo"]
        );
        assert_eq!(
            Action::Comment("hi".into()).args(&pr, Kind::Pull),
            ["pr", "comment", "7", "-R", "o/r", "--body", "hi"]
        );
        assert_eq!(
            Action::CloseAs(CloseReason::NotPlanned).args(&pr, Kind::Issue),
            [
                "issue",
                "close",
                "7",
                "-R",
                "o/r",
                "--reason",
                "not planned"
            ]
        );
        assert_eq!(
            Action::Reopen.args(&pr, Kind::Issue),
            ["issue", "reopen", "7", "-R", "o/r"]
        );
    }

    #[test]
    fn an_issue_reads_its_state_reason_and_comments() {
        let issue = PrRef {
            repo: "o/r".into(),
            number: 3,
        };
        let detail = parse_issue(
            issue,
            &json!({
                "title": "Crash on open",
                "state": "CLOSED",
                "stateReason": "NOT_PLANNED",
                "author": {"login": "ann"},
                "createdAt": "2026-11-01T00:00:00Z",
                "closedAt": "2026-11-03T00:00:00Z",
                "assignees": [{"login": "me"}],
                "labels": [{"name": "bug"}],
                "comments": [{"author": {"login": "me"}, "body": "Can't reproduce", "createdAt": "2026-11-02T00:00:00Z"}]
            }),
        );
        assert_eq!(detail.kind, Kind::Issue);
        assert_eq!(detail.state, State::NotPlanned);
        assert_eq!(detail.assignees, ["me"]);
        assert_eq!(detail.labels, ["bug"]);
        let kinds: Vec<_> = detail.timeline.iter().map(|e| &e.kind).collect();
        assert_eq!(
            kinds,
            [
                &EventKind::Opened,
                &EventKind::Comment {
                    body: "Can't reproduce".into()
                },
                &EventKind::Closed
            ]
        );
    }
}
