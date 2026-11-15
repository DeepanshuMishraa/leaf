//! Diffs of GitHub pull requests and GitLab merge requests, plus commits and
//! comparisons. Either forge's diff is read the same way; commenting and every
//! other write are still GitHub's alone.

use std::fmt::Write as _;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use crate::comment::{Anchor, Side};
use crate::diff::Repo;

/// A diff larger than this is refused rather than held in memory.
const MAX_BYTES: u64 = 96 * 1024 * 1024;

/// Whether a command answers `--version`, asked once per run.
fn installed(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

static GH: LazyLock<bool> = LazyLock::new(|| installed("gh"));

/// The GitLab CLI, asked first for a merge request's patch. It carries the
/// credentials it was signed in with, so a private project needs no token in
/// leaf's own environment; without it the page answers instead.
static GLAB: LazyLock<bool> = LazyLock::new(|| installed("glab"));

/// Which forge serves a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forge {
    GitHub,
    GitLab,
}

impl Forge {
    /// How the forge is written in a storage key.
    pub fn name(self) -> &'static str {
        match self {
            Forge::GitHub => "github",
            Forge::GitLab => "gitlab",
        }
    }

    /// How the forge writes its own name, for messages and labels.
    pub fn title(self) -> &'static str {
        match self {
            Forge::GitHub => "GitHub",
            Forge::GitLab => "GitLab",
        }
    }

    /// The path segment that names a pull request.
    fn pull_segment(self) -> &'static str {
        match self {
            Forge::GitHub => "pull",
            Forge::GitLab => "merge_requests",
        }
    }

    /// The forge a host names, when its name says so: `github.com`, or
    /// `gitlab.com` and the self-hosted instances that keep `gitlab` in their
    /// name. [`forge_of`] also asks `GITLAB_HOST`, for the rest.
    pub fn of_host(host: &str) -> Option<Forge> {
        let host = bare_host(host)?;
        if host == "github.com" {
            Some(Forge::GitHub)
        } else if host.split('.').any(|label| label == "gitlab") {
            Some(Forge::GitLab)
        } else {
            None
        }
    }
}

/// The forge a host serves: the one its name says, or the GitLab that
/// `GITLAB_HOST` names. A self-hosted instance on a name of its own is known
/// this way, and so is the only host its token is offered to.
pub fn forge_of(host: &str) -> Option<Forge> {
    forge_of_hosts(host, &configured_gitlab_hosts())
}

fn forge_of_hosts(host: &str, configured: &[String]) -> Option<Forge> {
    Forge::of_host(host).or_else(|| is_gitlab_host(host, configured).then_some(Forge::GitLab))
}

/// The GitLab hosts `GITLAB_HOST` names, comma or space separated, each a URL
/// or a bare host, so that more than one self-hosted instance can be named.
fn configured_gitlab_hosts() -> Vec<String> {
    std::env::var("GITLAB_HOST")
        .map(|list| parse_hosts(&list))
        .unwrap_or_default()
}

fn parse_hosts(list: &str) -> Vec<String> {
    list.split([',', ';', ' ']).filter_map(bare_host).collect()
}

/// A host on its own: lower case, without a scheme, credentials, a port or a
/// path. `user:password@host` names `host`, and `gitlab.com@evil.test` names
/// `evil.test`, which is where the request would actually go.
fn bare_host(host: &str) -> Option<String> {
    let host = host.trim().to_ascii_lowercase();
    let host = host
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let host = host.split(['/', ':']).next().unwrap_or("");
    (!host.is_empty()).then(|| host.to_owned())
}

/// The authority a link names, as written and lower case, without credentials
/// but with the port it listens on: that is where the request goes. The name
/// without the port is what decides the forge, in [`bare_host`].
fn authority_with_port(text: &str) -> Option<String> {
    let text = text.trim().to_ascii_lowercase();
    let name = text.split(':').next().unwrap_or("");
    let plain = !text.contains([' ', '\t']);
    (plain && !name.is_empty()).then_some(text)
}

/// The host and the path below it, from a remote over any of git's schemes:
/// `scheme://[user@]host[:port]/path`, or `git@host:path`.
pub fn host_and_path(url: &str) -> Option<(String, String)> {
    let (authority, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        // Without a scheme, git writes `git@host:path`.
        None => url.split_once(':')?,
    };
    let host = bare_host(authority)?;
    let path = path.trim_matches('/');
    (!path.is_empty()).then(|| (host, path.to_owned()))
}

/// Whether a host is a GitLab: by its name, or one `GITLAB_HOST` names. A
/// host that names itself GitHub never is, however `GITLAB_HOST` is written,
/// so a typo there can't send a GitLab token to GitHub.
fn is_gitlab_host(host: &str, configured: &[String]) -> bool {
    match Forge::of_host(host) {
        Some(Forge::GitLab) => true,
        Some(Forge::GitHub) => false,
        None => bare_host(host).is_some_and(|host| configured.contains(&host)),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GithubFailed {
    #[error(
        "That link isn't a pull request, merge request, commit or comparison: {url}. Use a link like https://github.com/owner/repo/pull/12 or https://gitlab.com/group/project/-/merge_requests/12."
    )]
    NotForge { url: String },
    #[error(
        "Could not reach {forge} ({cause}). Check your connection and the link, then try again."
    )]
    Network { forge: &'static str, cause: String },
    #[error("{forge} returned HTTP {status} while {what}. {hint}")]
    Http {
        forge: &'static str,
        status: u16,
        what: String,
        hint: String,
    },
    #[error(
        "{forge} returned a page rather than a diff for {url}. The project may be private, or the link may name something other than a merge request, a commit or a comparison."
    )]
    NotDiff { forge: &'static str, url: String },
    #[error(
        "Leaf opens {forge} diffs to read, and posting a comment to {forge} isn't supported yet. Post it on the merge request in your browser."
    )]
    Unsupported { forge: &'static str },
    #[error(
        "{forge} sent only part of the patch ({detail}), so leaf stopped rather than show a review that is missing files. Open it on the page in your browser, or install `glab` and sign in to that host."
    )]
    Partial { forge: &'static str, detail: String },
    #[error("{cause}")]
    Folded { cause: String },
    #[error(
        "GitHub rejected the comment: {detail} Check that the line is still part of the pull request, then try again."
    )]
    Post { detail: String },
    #[error(
        "No pull request found for {branch}. Open one on GitHub first, or comment on a pull request link directly."
    )]
    NoPullRequest { branch: String },
    #[error(
        "Posting to GitHub needs the GitHub CLI (`gh`) or a GH_TOKEN/GITHUB_TOKEN environment variable. Install it from https://cli.github.com, run `gh auth login`, then try again."
    )]
    NoAuth,
    #[error("`{tool} {command}` failed: {cause}")]
    Cli {
        tool: &'static str,
        command: String,
        cause: String,
    },
}

/// The repository page a diff comes from. `project` is GitHub's `owner/name`
/// or GitLab's `group/subgroup/project`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Pull {
        project: String,
        number: u64,
    },
    Commit {
        project: String,
        sha: String,
    },
    Compare {
        project: String,
        base: String,
        head: String,
    },
}

/// A page that holds a diff, and the forge and host serving it. The host is
/// carried rather than assumed so that a self-hosted GitLab works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub forge: Forge,
    pub host: String,
    pub page: Page,
}

impl Resource {
    /// A page of a GitHub repository.
    pub fn github(page: Page) -> Self {
        Resource {
            forge: Forge::GitHub,
            host: "github.com".to_owned(),
            page,
        }
    }

    pub fn parse_url(url: &str) -> Option<Self> {
        let url = url.trim();
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(url);
        let (authority, rest) = rest.split_once('/')?;
        // A link that carries credentials names one host and reaches another:
        // `gitlab.com@evil.test` connects to `evil.test`. It isn't a forge
        // page link, whatever the part before the `@` says, so it is refused.
        if authority.contains('@') {
            return None;
        }
        let host = authority_with_port(authority)?;
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        // A host that names its forge decides how the path is read, so a
        // GitHub ref that happens to contain `/-/` is still a GitHub ref.
        // Only a host that says nothing needs `/-/` to be told apart. A host
        // `GITLAB_HOST` names is read the way a host naming itself GitLab is.
        let named = forge_of(&host);
        let page = match named {
            Some(Forge::GitHub) => {
                let (owner, rest) = rest.split_once('/')?;
                let (name, tail) = rest.split_once('/')?;
                let project = format!("{}/{}", clean(owner)?, clean(name)?);
                page_of(Forge::GitHub, project, tail)?
            }
            Some(Forge::GitLab) => gitlab_page(rest)?,
            None => {
                // Nothing says which forge this is, so `/-/` does: it is the
                // separator GitLab alone has, and a page to read is all that
                // is wanted here -- no credential is offered to a host that
                // hasn't named itself.
                let (project, tail) = rest.split_once("/-/")?;
                page_of(Forge::GitLab, clean_project(project)?, tail)?
            }
        };
        Some(Resource {
            forge: named.unwrap_or(Forge::GitLab),
            host,
            page,
        })
    }

    /// The repository's path within the forge.
    pub fn project(&self) -> &str {
        match &self.page {
            Page::Pull { project, .. }
            | Page::Commit { project, .. }
            | Page::Compare { project, .. } => project,
        }
    }

    pub fn label(&self) -> String {
        let project = self.project();
        match &self.page {
            Page::Pull { number, .. } => {
                // Each forge writes a pull request's number its own way.
                let sign = match self.forge {
                    Forge::GitHub => '#',
                    Forge::GitLab => '!',
                };
                format!("{project}{sign}{number}")
            }
            Page::Commit { sha, .. } => {
                format!("{project}@{}", &sha[..sha.len().min(7)])
            }
            Page::Compare { base, head, .. } => format!("{project} {base}...{head}"),
        }
    }

    pub fn key(&self) -> String {
        let project = self.project();
        let page = match &self.page {
            Page::Pull { number, .. } => format!("{}/{number}", self.forge.pull_segment()),
            Page::Commit { sha, .. } => format!("commit/{sha}"),
            Page::Compare { base, head, .. } => format!("compare/{base}...{head}"),
        };
        match self.forge {
            // Unchanged from when GitLab wasn't read, so saved diffs still load.
            Forge::GitHub => format!("{project}/{page}"),
            // Two GitLab hosts can serve one project, so the host is part of
            // the key.
            Forge::GitLab => format!("{}/{project}/-/{page}", self.host),
        }
    }

    /// The GitHub API path that holds the diff.
    fn api_path(&self) -> String {
        let project = self.project();
        match &self.page {
            Page::Pull { number, .. } => format!("repos/{project}/pulls/{number}"),
            Page::Commit { sha, .. } => format!("repos/{project}/commits/{sha}"),
            Page::Compare { base, head, .. } => {
                format!("repos/{project}/compare/{base}...{head}")
            }
        }
    }

    fn web_url(&self) -> String {
        let project = self.project();
        let page = match &self.page {
            Page::Pull { number, .. } => {
                format!("{}/{number}", self.forge.pull_segment())
            }
            Page::Commit { sha, .. } => format!("commit/{sha}"),
            Page::Compare { base, head, .. } => format!("compare/{base}...{head}"),
        };
        match self.forge {
            Forge::GitHub => format!("https://{}/{project}/{page}", self.host),
            Forge::GitLab => format!("https://{}/{project}/-/{page}", self.host),
        }
    }

    fn diff_url(&self) -> String {
        // GitLab serves a merge request's and a commit's patch at `.diff`, but
        // a comparison's only at `?format=diff`; `.diff` there is the page.
        match (&self.forge, &self.page) {
            (Forge::GitLab, Page::Compare { .. }) => format!("{}?format=diff", self.web_url()),
            _ => format!("{}.diff", self.web_url()),
        }
    }
}

fn clean(part: &str) -> Option<String> {
    let part = part.trim();
    (!part.is_empty()).then(|| part.to_owned())
}

/// A GitLab path split into its project and the page below it. GitLab puts
/// `/-/` before every repository page, and still redirects the older form
/// without it (`group/project/merge_requests/12`), which people link to in
/// older issues and mail.
/// The page a GitLab path names, both as GitLab writes links now and as it
/// wrote them before `/-/`: `/group/project/merge_requests/1` still redirects
/// to `/-/merge_requests/1`, and people paste the older form from older
/// issues and mail.
fn gitlab_page(rest: &str) -> Option<Page> {
    if let Some((project, tail)) = rest.split_once("/-/") {
        return page_of(Forge::GitLab, clean_project(project)?, tail);
    }
    // Without `/-/` the project has to be found by trying each place a page
    // could start: a group may be called `commit`, and a branch may contain
    // `/merge_requests/`, so only the way the rest reads says which it is.
    let mut at: Vec<usize> = [
        "merge_requests",
        "merge_request",
        "commit",
        "commits",
        "compare",
    ]
    .into_iter()
    .flat_map(|kind| {
        let needle = format!("/{kind}/");
        rest.match_indices(&needle)
            .map(|(at, _)| at + 1)
            .collect::<Vec<_>>()
    })
    .collect();
    at.sort_unstable();
    at.into_iter().find_map(|at| {
        let (project, tail) = rest.split_at(at);
        page_of(Forge::GitLab, clean_project(project)?, tail)
    })
}

/// The page a path names below a project it is already known to sit in.
fn page_of(forge: Forge, project: String, tail: &str) -> Option<Page> {
    let mut parts = tail.split('/');
    let kind = parts.next()?;
    let pull = match forge {
        Forge::GitHub => kind == "pull" || kind == "pulls",
        Forge::GitLab => kind == "merge_requests" || kind == "merge_request",
    };
    if pull {
        return Some(Page::Pull {
            project,
            number: parts.next()?.trim_end_matches(".diff").parse().ok()?,
        });
    }
    if kind == "commit" || kind == "commits" {
        let sha = parts.next()?.trim_end_matches(".diff");
        if !is_sha(sha) {
            return None;
        }
        return Some(Page::Commit {
            project,
            sha: sha.to_owned(),
        });
    }
    if kind == "compare" {
        let range = parts.collect::<Vec<_>>().join("/");
        let (base, head) = range.trim_end_matches(".diff").split_once("...")?;
        return Some(Page::Compare {
            project,
            base: clean(base)?,
            head: clean(head)?,
        });
    }
    None
}

/// A repository path such as `owner/name` or `group/subgroup/project`, with no
/// empty segment. GitLab nests a project as deep as its groups are.
fn clean_project(project: &str) -> Option<String> {
    let project = project.trim().trim_matches('/');
    let whole = !project.is_empty() && project.split('/').all(|part| !part.is_empty());
    whole.then(|| project.to_owned())
}

/// The two halves of a GitHub project path, which is always `owner/name`.
fn halves(project: &str) -> (&str, &str) {
    project.split_once('/').unwrap_or(("", project))
}

fn is_sha(sha: &str) -> bool {
    (7..=40).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// A diff fetched from a forge, and where it came from.
pub struct Fetched {
    pub patch: String,
    pub resource: Resource,
}

/// Downloads the diff for a pull request, merge request, commit or comparison.
pub fn fetch(url: &str) -> Result<Fetched, GithubFailed> {
    let resource = Resource::parse_url(url).ok_or_else(|| GithubFailed::NotForge {
        url: url.to_owned(),
    })?;
    let patch = match resource.forge {
        Forge::GitHub => github_diff(&resource)?,
        Forge::GitLab => gitlab_diff(&resource)?,
    };
    Ok(Fetched { patch, resource })
}

/// A GitHub diff, asked for through `gh` first and then the API, so that a
/// signed-in `gh` answers for private repositories.
fn github_diff(resource: &Resource) -> Result<String, GithubFailed> {
    let path = resource.api_path();

    let mut gh_error = None;
    if *GH {
        match gh(&[
            "api",
            "--method",
            "GET",
            "-H",
            "Accept: application/vnd.github.v3.diff",
            &path,
        ]) {
            Ok(patch)
                if patch.trim().is_empty()
                    || patch.starts_with("diff --git")
                    || patch.starts_with("From ") =>
            {
                return Ok(patch);
            }
            Ok(other) => {
                gh_error = Some(format!(
                    "gh returned something that isn't a diff: {}",
                    first_line(&other)
                ));
            }
            Err(e) => gh_error = Some(e.to_string()),
        }
    }

    http_get(&resource.diff_url(), "application/vnd.github.v3.diff").map_err(|e| match gh_error {
        Some(gh) => GithubFailed::Network {
            forge: Forge::GitHub.title(),
            cause: format!("{}; gh said: {gh}", short_cause(&e)),
        },
        None => e,
    })
}

/// The `glab` command that prints a merge request's patch, when it can help:
/// the CLI is installed, the page is a merge request (`glab mr diff` is the
/// only command of its that prints a raw patch), and the host is one leaf
/// would give a GitLab token to, so a link pasted from anywhere else starts no
/// conversation at all. `None` means "ask the page instead".
fn glab_command(
    resource: &Resource,
    installed: bool,
    configured: &[String],
) -> Option<Vec<String>> {
    if !installed || !is_gitlab_host(&resource.host, configured) {
        return None;
    }
    let Page::Pull { project, number } = &resource.page else {
        return None;
    };
    Some(vec![
        "mr".to_owned(),
        "diff".to_owned(),
        number.to_string(),
        // As a URL, so a self-hosted instance is asked on its own host and not
        // on whichever one `glab` happens to default to.
        "--repo".to_owned(),
        format!("https://{}/{project}", resource.host),
        // An unmodified patch, and no colour whether or not it's a terminal.
        "--raw".to_owned(),
        "--color".to_owned(),
        "never".to_owned(),
    ])
}

/// A GitLab diff: `glab` first when it's there, then the patch the page itself
/// serves, with `GITLAB_TOKEN` when the project is private. Both print the
/// same patch; the first is the one that already knows your sign-in.
fn gitlab_diff(resource: &Resource) -> Result<String, GithubFailed> {
    let mut said: Vec<(&str, String)> = Vec::new();
    if let Some(args) = glab_command(resource, *GLAB, &configured_gitlab_hosts()) {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match glab_in(&args) {
            Ok(patch) if patch.trim().is_empty() || patch.starts_with("diff --git") => {
                return Ok(patch);
            }
            Ok(other) => said.push((
                "glab",
                format!(
                    "it returned something that isn't a diff: {}",
                    first_line(&other)
                ),
            )),
            Err(e) => said.push(("glab", short_cause(&e))),
        }
    }

    // A token is only ever offered to a host that names itself GitLab or that
    // `GITLAB_HOST` names, and it is spent on the API: the web routes that
    // serve a patch refuse a personal access token, answering with the sign-in
    // page, so a private project is read here instead.
    let refused = match gitlab_token(&resource.host) {
        Some(_) => match gitlab_api_diff(resource) {
            Ok(patch) => return Ok(patch),
            Err(e) => Some(e),
        },
        None => None,
    };

    let url = resource.diff_url();
    let page =
        http_get_forge(&url, "text/plain", Forge::GitLab, &resource.host).and_then(|patch| {
            // GitLab answers a project it won't show with a page rather than an
            // error, and a token doesn't change that: the sign-in page comes back.
            if patch.trim().is_empty() || patch.starts_with("diff --git") {
                Ok(patch)
            } else {
                Err(GithubFailed::NotDiff {
                    forge: Forge::GitLab.title(),
                    url,
                })
            }
        });

    match (refused, page) {
        // With a token the API is the one that knows why, so it leads.
        (Some(api), Err(page)) => Err(said_so(api, &[("the page", page)], &said)),
        (Some(_), Ok(patch)) => Ok(patch),
        (None, Err(page)) => Err(said_so(page, &[], &said)),
        (None, Ok(patch)) => Ok(patch),
    }
}

/// The API request that carries `resource`'s patch, and what it answers with:
/// a merge request's raw patch is text, and every other page is JSON.
fn gitlab_api_request(resource: &Resource) -> (String, &'static str) {
    let project = encode(resource.project());
    let base = format!("https://{}/api/v4/projects/{project}", resource.host);
    match &resource.page {
        Page::Pull { number, .. } => (
            format!("{base}/merge_requests/{number}/raw_diffs"),
            "text/plain",
        ),
        Page::Commit { sha, .. } => (
            format!("{base}/repository/commits/{sha}/diff"),
            "application/json",
        ),
        Page::Compare {
            base: from,
            head: to,
            ..
        } => (
            format!(
                "{base}/repository/compare?from={}&to={}",
                encode(from),
                encode(to)
            ),
            "application/json",
        ),
    }
}

/// The patch GitLab's API serves for `resource`, which is the only way in when
/// the project is private and no `glab` is signed in to it.
fn gitlab_api_diff(resource: &Resource) -> Result<String, GithubFailed> {
    let (url, accept) = gitlab_api_request(resource);
    match http_get_forge(&url, accept, Forge::GitLab, &resource.host) {
        // GitLab 16.9 gave a merge request's raw patch a route of its own;
        // before that, the list of changes says the same in JSON.
        Err(GithubFailed::Http { status: 404, .. }) if accept == "text/plain" => {
            let project = encode(resource.project());
            let base = format!("https://{}/api/v4/projects/{project}", resource.host);
            let Page::Pull { number, .. } = &resource.page else {
                return Err(GithubFailed::NotDiff {
                    forge: Forge::GitLab.title(),
                    url,
                });
            };
            let url = format!("{base}/merge_requests/{number}/changes");
            gitlab_diff_json(&url, resource)
        }
        Ok(patch) if accept == "text/plain" => Ok(patch),
        Ok(body) => patch_from_json(&body, &url),
        Err(e) => Err(e),
    }
}

/// A patch rebuilt from the JSON that lists a page's changed files, which is
/// how the API answers every page but a merge request.
fn gitlab_diff_json(url: &str, resource: &Resource) -> Result<String, GithubFailed> {
    let body = http_get_forge(url, "application/json", Forge::GitLab, &resource.host)?;
    patch_from_json(&body, url)
}

/// A patch from the JSON the API answered with, or a refusal to read it as
/// one: an error page can come back as a 200 too.
fn patch_from_json(body: &str, url: &str) -> Result<String, GithubFailed> {
    let Changed { changes, cut } = changes_of(body).ok_or_else(|| GithubFailed::NotDiff {
        forge: Forge::GitLab.title(),
        url: url.to_owned(),
    })?;
    // A review of part of a patch is worse than none: what isn't shown is
    // indistinguishable from what isn't there.
    if let Some(detail) = cut {
        return Err(GithubFailed::Partial {
            forge: Forge::GitLab.title(),
            detail,
        });
    }
    Ok(patch_from_changes(&changes))
}

/// The changed files out of whichever shape GitLab answered in: a commit's
/// diff is the list itself, a comparison's is under `diffs`, and a merge
/// request's under `changes`.
///
/// A big page comes back cut: GitLab caps a merge request at 1000 changed
/// files and says so with `overflow`, and a comparison that runs out of time
/// says `compare_timeout`. Reading either as complete would show a review
/// with files quietly missing, so what the answer admits is passed on.
fn changes_of(body: &str) -> Option<Changed> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let flag = |key: &str| value.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    let cut = if flag("overflow") || flag("compare_timeout") {
        let count = match value.get("changes_count").and_then(|v| v.as_str()) {
            Some(count) if count.ends_with('+') => format!("at least {count} files"),
            Some(count) => format!("{count} files"),
            None => "more files than it sends".to_owned(),
        };
        Some(format!(
            "it sent {} of {count}",
            value
                .get("changes")
                .or_else(|| value.get("diffs"))
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len)
        ))
    } else {
        None
    };
    let changes = if let Some(list) = value.as_array() {
        list.clone()
    } else {
        ["changes", "diffs"]
            .into_iter()
            .find_map(|key| value.get(key)?.as_array().cloned())?
    };
    Some(Changed { changes, cut })
}

/// The changed files an answer listed, and what it admitted was left out.
struct Changed {
    changes: Vec<serde_json::Value>,
    cut: Option<String>,
}

/// A patch built the way Git writes one, from the files the API listed. The
/// hunks are already a patch minus its headers, so only those are written.
fn patch_from_changes(changes: &[serde_json::Value]) -> String {
    let mut patch = String::new();
    for change in changes {
        let text = |key: &str| {
            change
                .get(key)
                .and_then(|value| value.as_str())
                .unwrap_or("")
        };
        let flag = |key: &str| {
            change
                .get(key)
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        };
        let old = text("old_path");
        let new = text("new_path");
        let added = flag("new_file");
        let deleted = flag("deleted_file");
        let _ = writeln!(
            patch,
            "diff --git {} {}",
            crate::diff::quote_path(&format!("a/{old}")),
            crate::diff::quote_path(&format!("b/{new}"))
        );
        let mode = |key: &str| {
            let mode = text(key);
            if mode.is_empty() {
                "100644".to_owned()
            } else {
                mode.to_owned()
            }
        };
        if added {
            let _ = writeln!(patch, "new file mode {}", mode("b_mode"));
        } else if deleted {
            let _ = writeln!(patch, "deleted file mode {}", mode("a_mode"));
        } else {
            // A file made executable changes no lines at all: the two modes
            // are the whole change, and without them it reads as an untouched
            // file. Git writes them before the rename for the same reason.
            let (before, after) = (text("a_mode"), text("b_mode"));
            if !before.is_empty() && !after.is_empty() && before != after {
                let _ = writeln!(patch, "old mode {before}");
                let _ = writeln!(patch, "new mode {after}");
            }
            if flag("renamed_file") {
                let _ = writeln!(patch, "rename from {old}");
                let _ = writeln!(patch, "rename to {new}");
            }
        }
        let _ = writeln!(
            patch,
            "--- {}",
            if added {
                "/dev/null".to_owned()
            } else {
                crate::diff::quote_path(&format!("a/{old}"))
            }
        );
        let _ = writeln!(
            patch,
            "+++ {}",
            if deleted {
                "/dev/null".to_owned()
            } else {
                crate::diff::quote_path(&format!("b/{new}"))
            }
        );
        let diff = text("diff");
        if diff.trim().is_empty() {
            // A rename or a mode change carries its whole meaning in the lines
            // above. Anything else emptied here is a file GitLab couldn't show,
            // and it says so rather than look untouched.
            let moved =
                flag("renamed_file") || (text("a_mode") != text("b_mode") && !added && !deleted);
            if !moved {
                let why = if flag("too_large") {
                    "leaf: too large for GitLab to send its changes"
                } else {
                    "leaf: GitLab sent no changes for this file"
                };
                let _ = writeln!(patch, "{why}");
            }
        } else {
            patch.push_str(diff);
            if !diff.ends_with('\n') {
                patch.push('\n');
            }
        }
    }
    patch
}

/// Percent-encodes a project's path or a ref, `/` included: GitLab wants the
/// project as one segment, `group%2Fproject`.
fn encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// Folds what the other routes said into the failure that ended the attempt,
/// so one message carries every place the patch was looked for. The failure
/// keeps its kind: an HTTP status mustn't come back as a connection problem
/// with advice about cables.
fn said_so(
    failed: GithubFailed,
    others: &[(&str, GithubFailed)],
    said: &[(&str, String)],
) -> GithubFailed {
    let mut tail = String::new();
    for (who, other) in others {
        tail.push_str(&format!("; {who} said: {}", short_cause(other)));
    }
    for (who, other) in said {
        tail.push_str(&format!("; {who} said: {other}"));
    }
    if tail.is_empty() {
        return failed;
    }
    match failed {
        GithubFailed::Http {
            forge,
            status,
            what,
            hint,
        } => GithubFailed::Http {
            forge,
            status,
            what,
            hint: format!("{hint}{tail}"),
        },
        GithubFailed::Network { forge, cause } => GithubFailed::Network {
            forge,
            cause: format!("{cause}{tail}"),
        },
        // Everything else has no place to put a tail, so it is carried whole:
        // dropping what the other routes said is how a `glab` failure turns
        // into an unrelated complaint about the page.
        other => GithubFailed::Folded {
            cause: format!("{}{tail}", short_cause(&other)),
        },
    }
}

struct Target {
    owner: String,
    repo: String,
    number: u64,
    head: String,
    label: String,
}

/// Posts a review comment on the pull request behind `resource`, or on the
/// current branch's pull request when `resource` isn't a pull request itself.
pub fn post_review(
    resource: Option<&Resource>,
    repo: Option<&Repo>,
    branch: Option<&str>,
    anchor: &Anchor,
    body: &str,
) -> Result<String, GithubFailed> {
    if let Some(resource) = resource
        && resource.forge != Forge::GitHub
    {
        return Err(GithubFailed::Unsupported {
            forge: resource.forge.title(),
        });
    }
    let target = target(resource, repo, branch)?;
    let side = match anchor.side {
        Side::Old => "LEFT",
        Side::New => "RIGHT",
    };
    let path = format!(
        "repos/{}/{}/pulls/{}/comments",
        target.owner, target.repo, target.number
    );

    let mut args: Vec<String> = vec![
        "api".to_owned(),
        "--method".to_owned(),
        "POST".to_owned(),
        path,
        "-f".to_owned(),
        format!("body={body}"),
        "-f".to_owned(),
        format!("commit_id={}", target.head),
        "-f".to_owned(),
        format!("path={}", anchor.file),
        "-f".to_owned(),
        format!("side={side}"),
        "-F".to_owned(),
        format!("line={}", anchor.end_line),
    ];
    if anchor.start_line != anchor.end_line {
        args.push("-F".to_owned());
        args.push(format!("start_line={}", anchor.start_line));
        args.push("-f".to_owned());
        args.push(format!("start_side={side}"));
    }

    let mut gh_error = None;
    if *GH {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match gh(&refs) {
            Ok(_) => return Ok(target.label),
            Err(e) => gh_error = Some(e),
        }
    }
    if token().is_some() {
        http_post_comment(&target, anchor, body, side)?;
        return Ok(target.label);
    }
    Err(gh_error.unwrap_or(GithubFailed::NoAuth))
}

fn target(
    resource: Option<&Resource>,
    repo: Option<&Repo>,
    branch: Option<&str>,
) -> Result<Target, GithubFailed> {
    match resource {
        Some(Resource {
            page: Page::Pull { project, number },
            ..
        }) => {
            let (owner, name) = halves(project);
            pull_target(owner, name, *number)
        }
        Some(Resource {
            page: Page::Commit { project, sha },
            ..
        }) => {
            let (owner, name) = halves(project);
            match commit_pull_request(owner, name, sha)? {
                Some(number) => pull_target(owner, name, number),
                None => branch_target(repo, branch),
            }
        }
        Some(Resource {
            page: Page::Compare { .. },
            ..
        })
        | None => branch_target(repo, branch),
    }
}

fn pull_target(owner: &str, repo: &str, number: u64) -> Result<Target, GithubFailed> {
    let path = format!("repos/{owner}/{repo}/pulls/{number}");
    let mut head = None;
    if *GH && let Ok(json) = gh(&["api", &path]) {
        head = json_head_sha(&json);
    }
    let head = match head {
        Some(head) => head,
        None => {
            let json = http_get(&format!("https://api.github.com/{path}"), JSON_ACCEPT)?;
            json_head_sha(&json).ok_or_else(|| GithubFailed::NoPullRequest {
                branch: format!("{owner}/{repo}#{number}"),
            })?
        }
    };
    Ok(Target {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        number,
        head,
        label: format!("{owner}/{repo}#{number}"),
    })
}

fn commit_pull_request(owner: &str, repo: &str, sha: &str) -> Result<Option<u64>, GithubFailed> {
    let path = format!("repos/{owner}/{repo}/commits/{sha}/pulls");
    if *GH && let Ok(json) = gh(&["api", &path]) {
        return Ok(json_number_after(&json, "number"));
    }
    let json = http_get(&format!("https://api.github.com/{path}"), JSON_ACCEPT)?;
    Ok(json_number_after(&json, "number"))
}

fn branch_target(repo: Option<&Repo>, branch: Option<&str>) -> Result<Target, GithubFailed> {
    let Some(repo) = repo else {
        return Err(GithubFailed::NoPullRequest {
            branch: branch.unwrap_or("this diff").to_owned(),
        });
    };
    if !*GH {
        return Err(GithubFailed::NoAuth);
    }
    let text = gh_in(
        Some(&repo.root),
        &[
            "pr",
            "view",
            "--json",
            "url,headRefOid",
            "--jq",
            ".url + \" \" + .headRefOid",
        ],
    )
    .map_err(|e| {
        let cause = e.to_string();
        if cause.contains("no pull request") {
            GithubFailed::NoPullRequest {
                branch: branch.unwrap_or("this branch").to_owned(),
            }
        } else {
            e
        }
    })?;
    let (url, head) = text.split_once(' ').ok_or(GithubFailed::NoPullRequest {
        branch: branch.unwrap_or("this branch").to_owned(),
    })?;
    let Some(Resource {
        page: Page::Pull { project, number },
        ..
    }) = Resource::parse_url(url)
    else {
        return Err(GithubFailed::NoPullRequest {
            branch: branch.unwrap_or("this branch").to_owned(),
        });
    };
    let (owner, name) = halves(&project);
    Ok(Target {
        owner: owner.to_owned(),
        repo: name.to_owned(),
        number,
        head: head.trim().to_owned(),
        label: format!("{project}#{number}"),
    })
}

const JSON_ACCEPT: &str = "application/vnd.github+json";

/// A GitHub request, authenticated with the GitHub CLI's token when there is
/// one.
fn http_get(url: &str, accept: &str) -> Result<String, GithubFailed> {
    http_get_forge(url, accept, Forge::GitHub, "github.com")
}

/// A request to `forge`, carrying that forge's token and no other: a GitHub
/// token must never be offered to a host the user only pasted a link from.
fn http_get_forge(
    url: &str,
    accept: &str,
    forge: Forge,
    host: &str,
) -> Result<String, GithubFailed> {
    let mut request = ureq::get(url)
        .header("user-agent", "leaf-reader (diff)")
        .header("accept", accept);
    match forge {
        Forge::GitHub => {
            if let Some(token) = token() {
                request = request.header("authorization", &format!("Bearer {token}"));
            }
        }
        Forge::GitLab => {
            if let Some(token) = gitlab_token(host) {
                request = request.header("private-token", &token);
            }
        }
    }
    let response = request
        .call()
        .map_err(|e| http_failure(e, "fetching the diff", forge))?;
    let bytes = response
        .into_body()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()
        .map_err(|e| GithubFailed::Network {
            forge: forge.title(),
            cause: e.to_string(),
        })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn http_post_comment(
    target: &Target,
    anchor: &Anchor,
    body: &str,
    side: &str,
) -> Result<(), GithubFailed> {
    let token = token().ok_or(GithubFailed::NoAuth)?;
    let url = format!(
        "https://api.github.com/repos/{}/{}/pulls/{}/comments",
        target.owner, target.repo, target.number
    );
    let mut json = format!(
        "{{\"body\":\"{}\",\"commit_id\":\"{}\",\"path\":\"{}\",\"line\":{},\"side\":\"{side}\"",
        json_escape(body),
        json_escape(&target.head),
        json_escape(&anchor.file),
        anchor.end_line
    );
    if anchor.start_line != anchor.end_line {
        let _ = std::fmt::Write::write_fmt(
            &mut json,
            format_args!(
                ",\"start_line\":{},\"start_side\":\"{side}\"",
                anchor.start_line
            ),
        );
    }
    json.push('}');

    let response = ureq::post(&url)
        .header("user-agent", "leaf-reader (diff)")
        .header("accept", JSON_ACCEPT)
        .header("x-github-api-version", "2022-11-28")
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .send(json.as_str());
    match response {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(status)) => Err(GithubFailed::Post {
            detail: format!("HTTP {status}. {}", hint(status, Forge::GitHub)),
        }),
        Err(other) => Err(GithubFailed::Network {
            forge: Forge::GitHub.title(),
            cause: other.to_string(),
        }),
    }
}

fn gh(args: &[&str]) -> Result<String, GithubFailed> {
    gh_in(None, args)
}

fn gh_in(dir: Option<&std::path::Path>, args: &[&str]) -> Result<String, GithubFailed> {
    tool_in("gh", dir, args)
}

fn glab_in(args: &[&str]) -> Result<String, GithubFailed> {
    tool_in("glab", None, args)
}

/// Runs a forge's CLI. Its own words are kept, so a signed-out `glab` explains
/// itself in the message the fallback carries.
fn tool_in(
    tool: &'static str,
    dir: Option<&std::path::Path>,
    args: &[&str],
) -> Result<String, GithubFailed> {
    let mut command = Command::new(tool);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command.output().map_err(|e| GithubFailed::Cli {
        tool,
        command: args.join(" "),
        cause: e.to_string(),
    })?;
    if !output.status.success() {
        let cause = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(GithubFailed::Cli {
            tool,
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

fn http_failure(error: ureq::Error, what: &str, forge: Forge) -> GithubFailed {
    match error {
        ureq::Error::StatusCode(status) => GithubFailed::Http {
            forge: forge.title(),
            status,
            what: what.to_owned(),
            hint: hint(status, forge),
        },
        other => GithubFailed::Network {
            forge: forge.title(),
            cause: other.to_string(),
        },
    }
}

fn hint(status: u16, forge: Forge) -> String {
    let auth = match forge {
        Forge::GitHub => "Run `gh auth login`, or set GH_TOKEN",
        Forge::GitLab => {
            "Set GITLAB_TOKEN to a personal access token with `read_api` -- and GITLAB_HOST to a self-hosted instance's name -- or check the one that's set"
        }
    };
    match status {
        401 | 403 => format!(
            "The project may be private or {} may be rate limiting. {auth}, then try again.",
            forge.title()
        ),
        404 => {
            format!("The link may be wrong, or the project may be private. {auth}, then try again.")
        }
        422 => "The line may no longer be part of the pull request's diff.".to_owned(),
        429 => format!(
            "{} is rate limiting requests. Wait a minute, then try again.",
            forge.title()
        ),
        _ => "Try again in a moment.".to_owned(),
    }
}

fn token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn gitlab_token(host: &str) -> Option<String> {
    let token = ["GITLAB_TOKEN", "GL_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })?;
    is_gitlab_host(host, &configured_gitlab_hosts()).then_some(token)
}

/// What a failure said, without the forge it already named and without the
/// advice: a message that folds this in carries one piece of advice already,
/// and repeating it reads as two different instructions.
fn short_cause(failed: &GithubFailed) -> String {
    match failed {
        GithubFailed::Network { cause, .. } => cause.clone(),
        GithubFailed::Http { status, what, .. } => format!("HTTP {status} while {what}"),
        other => other.to_string(),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

fn json_head_sha(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("head")?.get("sha")?.as_str().map(str::to_owned)
}

fn json_number_after(text: &str, key: &str) -> Option<u64> {
    let quoted = format!("\"{key}\"");
    let at = text.find(&quoted)?;
    let rest = &text[at + quoted.len()..];
    let colon = rest.find(':')?;
    let digits: String = rest[colon + 1..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

pub(crate) fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = std::fmt::Write::write_fmt(&mut out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(project: &str, number: u64) -> Page {
        Page::Pull {
            project: project.into(),
            number,
        }
    }

    fn github(page: Page) -> Option<Resource> {
        Some(Resource::github(page))
    }

    fn gitlab(host: &str, page: Page) -> Option<Resource> {
        Some(Resource {
            forge: Forge::GitLab,
            host: host.into(),
            page,
        })
    }

    #[test]
    fn github_pages_parse() {
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/pull/12"),
            github(pull("o/r", 12))
        );
        assert_eq!(
            Resource::parse_url("http://github.com/o/r/pull/12/files#diff-abc"),
            github(pull("o/r", 12))
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/pull/12.diff"),
            github(pull("o/r", 12))
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/commit/0123456789abcdef"),
            github(Page::Commit {
                project: "o/r".into(),
                sha: "0123456789abcdef".into()
            })
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/compare/main...feat?expand=1"),
            github(Page::Compare {
                project: "o/r".into(),
                base: "main".into(),
                head: "feat".into()
            })
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/compare/main...feat/pr-viewer"),
            github(Page::Compare {
                project: "o/r".into(),
                base: "main".into(),
                head: "feat/pr-viewer".into()
            })
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/compare/release/1.0...fix/x?expand=1"),
            github(Page::Compare {
                project: "o/r".into(),
                base: "release/1.0".into(),
                head: "fix/x".into()
            })
        );
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/compare/main...feat/x.diff"),
            github(Page::Compare {
                project: "o/r".into(),
                base: "main".into(),
                head: "feat/x".into()
            })
        );
        assert_eq!(Resource::parse_url("https://github.com/o/r/issues/1"), None);
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/commit/not-a-sha"),
            None
        );
    }

    #[test]
    fn gitlab_merge_requests_parse() {
        assert_eq!(
            Resource::parse_url("https://gitlab.com/o/r/-/merge_requests/12"),
            gitlab("gitlab.com", pull("o/r", 12))
        );
        assert_eq!(
            Resource::parse_url("https://gitlab.com/o/r/-/merge_requests/12.diff"),
            gitlab("gitlab.com", pull("o/r", 12))
        );
        assert_eq!(
            Resource::parse_url("http://gitlab.com/o/r/-/merge_requests/12/diffs#note_1"),
            gitlab("gitlab.com", pull("o/r", 12))
        );
        // A self-hosted GitLab is named by its own host, and its links say
        // which forge they belong to by their `/-/`.
        assert_eq!(
            Resource::parse_url("https://git.example.com/team/api/-/merge_requests/7"),
            gitlab("git.example.com", pull("team/api", 7))
        );
        // GitLab projects nest as deep as their groups do.
        assert_eq!(
            Resource::parse_url("https://gitlab.com/a/b/c/-/merge_requests/3"),
            gitlab("gitlab.com", pull("a/b/c", 3))
        );
        assert_eq!(
            Resource::parse_url("https://gitlab.com/o/r/-/commit/0123456789abcdef"),
            gitlab(
                "gitlab.com",
                Page::Commit {
                    project: "o/r".into(),
                    sha: "0123456789abcdef".into()
                }
            )
        );
        assert_eq!(
            Resource::parse_url("https://gitlab.com/o/r/-/compare/main...feat"),
            gitlab(
                "gitlab.com",
                Page::Compare {
                    project: "o/r".into(),
                    base: "main".into(),
                    head: "feat".into()
                }
            )
        );
        // GitLab has no `/pull/`, and only GitHub serves the plain paths.
        assert_eq!(Resource::parse_url("https://gitlab.com/o/r/pull/1"), None);
        assert_eq!(Resource::parse_url("https://example.com/o/r/pull/1"), None);
        assert_eq!(
            Resource::parse_url("https://gitlab.com/o/r/-/issues/1"),
            None
        );
    }

    #[test]
    fn labels_and_diff_urls_are_built_from_the_page() {
        let pull = Resource::github(Page::Pull {
            project: "o/r".into(),
            number: 12,
        });
        assert_eq!(pull.label(), "o/r#12");
        assert_eq!(pull.key(), "o/r/pull/12");
        assert_eq!(pull.diff_url(), "https://github.com/o/r/pull/12.diff");
        let commit = Resource::github(Page::Commit {
            project: "o/r".into(),
            sha: "0123456789abcdef".into(),
        });
        assert_eq!(commit.label(), "o/r@0123456");
    }

    #[test]
    fn gitlab_asks_for_the_patch_the_way_each_page_serves_it() {
        let mr = Resource {
            forge: Forge::GitLab,
            host: "gitlab.com".into(),
            page: pull("o/r", 12),
        };
        assert_eq!(
            mr.diff_url(),
            "https://gitlab.com/o/r/-/merge_requests/12.diff"
        );
        let commit = Resource {
            forge: Forge::GitLab,
            host: "gitlab.com".into(),
            page: Page::Commit {
                project: "o/r".into(),
                sha: "0123456789abcdef".into(),
            },
        };
        assert_eq!(
            commit.diff_url(),
            "https://gitlab.com/o/r/-/commit/0123456789abcdef.diff"
        );
        // A comparison's `.diff` is the page itself; its patch is the format.
        let compare = Resource {
            forge: Forge::GitLab,
            host: "gitlab.com".into(),
            page: Page::Compare {
                project: "o/r".into(),
                base: "main".into(),
                head: "feat".into(),
            },
        };
        assert_eq!(
            compare.diff_url(),
            "https://gitlab.com/o/r/-/compare/main...feat?format=diff"
        );
        // A link's `http://` is upgraded, so a token never travels in the clear.
        assert_eq!(
            Resource::parse_url("http://gitlab.com/o/r/-/merge_requests/12")
                .unwrap()
                .diff_url(),
            "https://gitlab.com/o/r/-/merge_requests/12.diff"
        );
        // GitHub's comparison still reads its patch from `.diff`.
        assert_eq!(
            Resource::github(Page::Compare {
                project: "o/r".into(),
                base: "main".into(),
                head: "feat".into()
            })
            .diff_url(),
            "https://github.com/o/r/compare/main...feat.diff"
        );
    }

    #[test]
    fn a_branch_that_names_a_page_does_not_steal_the_comparison() {
        // The project ends where the page begins, and a branch may itself
        // contain `/merge_requests/` or `/commit/`; the range is what decides.
        let resource =
            Resource::parse_url("https://gitlab.com/o/r/compare/main...feature/merge_requests/1")
                .expect("a comparison of two branches");
        assert_eq!(
            resource.page,
            Page::Compare {
                project: "o/r".to_owned(),
                base: "main".to_owned(),
                head: "feature/merge_requests/1".to_owned(),
            }
        );
        let resource =
            Resource::parse_url("https://gitlab.com/o/r/compare/main...feature/commit/1")
                .expect("a comparison of two branches, the second one named commit");
        assert_eq!(
            resource.page,
            Page::Compare {
                project: "o/r".to_owned(),
                base: "main".to_owned(),
                head: "feature/commit/1".to_owned(),
            }
        );
        // A group called `commit` is still found, since a commit is a sha and
        // `b` is not one.
        let resource = Resource::parse_url("https://gitlab.com/a/commit/b/merge_requests/3")
            .expect("a group named commit");
        assert_eq!(
            resource.page,
            Page::Pull {
                project: "a/commit/b".to_owned(),
                number: 3,
            }
        );
    }

    #[test]
    fn a_host_named_gitlab_reads_the_older_link_form() {
        // GitLab still redirects `/-/`-less links, and a name that says GitLab
        // is enough to read one, whether it says so itself or by `GITLAB_HOST`.
        let resource = Resource::parse_url("https://gitlab.example.com/o/r/merge_requests/7")
            .expect("an older merge request link on a named host");
        assert_eq!(resource.forge, Forge::GitLab);
        assert_eq!(
            resource.page,
            Page::Pull {
                project: "o/r".to_owned(),
                number: 7,
            }
        );
        // The link is asked for as GitLab writes it now, whatever form it came in.
        assert_eq!(
            resource.web_url(),
            "https://gitlab.example.com/o/r/-/merge_requests/7"
        );
        // A host that names no forge still needs `/-/` to be read at all.
        assert_eq!(
            Resource::parse_url("https://code.example.com/o/r/merge_requests/7"),
            None
        );
    }

    #[test]
    fn a_link_that_carries_credentials_is_refused() {
        // `gitlab.com@evil.test` reaches evil.test, so a link that says
        // gitlab.com must not be read as one -- nor be handed a token.
        for url in [
            "https://gitlab.com@evil.test/x/-/merge_requests/1",
            "https://github.com@evil.test/o/r/pull/1",
            "https://user:password@gitlab.com/a/b/-/merge_requests/1",
            "https://oauth2:token@gitlab.com/a/b/-/merge_requests/1",
        ] {
            assert_eq!(Resource::parse_url(url), None, "{url}");
        }
        // The host is what a link would reach, with or without credentials.
        assert_eq!(bare_host("gitlab.com@evil.test"), Some("evil.test".into()));
        assert_eq!(
            bare_host("user:password@gitlab.com"),
            Some("gitlab.com".into())
        );
        // A remote may keep its credentials; the project is still named.
        assert_eq!(
            host_and_path("https://user:secret@gitlab.example.com/o/r.git"),
            Some(("gitlab.example.com".into(), "o/r.git".into()))
        );
        assert_eq!(
            host_and_path("ssh://git@gitlab.example.com:2222/group/p.git"),
            Some(("gitlab.example.com".into(), "group/p.git".into()))
        );
        assert_eq!(host_and_path("https://"), None);
        assert_eq!(host_and_path(""), None);
    }

    #[test]
    fn a_host_that_names_its_forge_decides_before_the_path() {
        // A GitHub ref may contain `/-/`, and that doesn't make it a GitLab.
        assert_eq!(
            Resource::parse_url("https://github.com/o/r/compare/main...feature/-/start"),
            github(Page::Compare {
                project: "o/r".into(),
                base: "main".into(),
                head: "feature/-/start".into(),
            })
        );
        // A GitLab page is still read as one, and never as GitHub's shape.
        assert_eq!(
            Resource::parse_url("https://gitlab.com/a/b/-/merge_requests/9"),
            gitlab("gitlab.com", pull("a/b", 9))
        );
        assert_eq!(Resource::parse_url("https://gitlab.com/o/r/pull/1"), None);
    }

    #[test]
    fn an_older_gitlab_link_is_read_without_its_dash() {
        // GitLab 301s these to the `/-/` form, so the same page opens.
        assert_eq!(
            Resource::parse_url("https://gitlab.com/group/project/merge_requests/12"),
            gitlab("gitlab.com", pull("group/project", 12))
        );
        assert_eq!(
            Resource::parse_url("https://gitlab.example.com/group/sub/project/merge_requests/3"),
            gitlab("gitlab.example.com", pull("group/sub/project", 3))
        );
        assert_eq!(
            Resource::parse_url("https://gitlab.com/o/r/commit/abcdef1"),
            gitlab(
                "gitlab.com",
                Page::Commit {
                    project: "o/r".into(),
                    sha: "abcdef1".into(),
                }
            )
        );
        assert_eq!(
            Resource::parse_url("https://gitlab.com/o/r/compare/v1...v2"),
            gitlab(
                "gitlab.com",
                Page::Compare {
                    project: "o/r".into(),
                    base: "v1".into(),
                    head: "v2".into(),
                }
            )
        );
        // A group named for a page kind doesn't hide where the project ends.
        assert_eq!(
            Resource::parse_url("https://gitlab.com/a/commit/b/merge_requests/3"),
            gitlab("gitlab.com", pull("a/commit/b", 3))
        );
        // Without `/-/` and without a kind, there's no page to open.
        assert_eq!(Resource::parse_url("https://gitlab.com/o/r"), None);
    }

    #[test]
    fn a_self_hosted_instance_keeps_its_port() {
        // A port is where the request goes, so it stays in the URL asked for,
        // while the name without it decides the forge and the token.
        let resource =
            Resource::parse_url("https://gitlab.example.com:8443/g/p/-/merge_requests/3")
                .expect("a merge request on its own port");
        assert_eq!(resource.host, "gitlab.example.com:8443");
        assert_eq!(
            resource.web_url(),
            "https://gitlab.example.com:8443/g/p/-/merge_requests/3"
        );
        assert_eq!(
            resource.diff_url(),
            "https://gitlab.example.com:8443/g/p/-/merge_requests/3.diff"
        );
        assert!(is_gitlab_host(
            &resource.host,
            &parse_hosts("gitlab.example.com")
        ));
        // Two instances of one project on different ports keep their own notes.
        let other = Resource::parse_url("https://gitlab.example.com/g/p/-/merge_requests/3")
            .expect("the same project on the default port");
        assert_ne!(resource.key(), other.key());
        // A link that says only a port names no host.
        assert_eq!(
            Resource::parse_url("https://:8443/g/p/-/merge_requests/3"),
            None
        );
        assert_eq!(
            Resource::parse_url("https:// /g/p/-/merge_requests/3"),
            None
        );
    }

    #[test]
    fn a_fallback_does_not_say_reach_twice() {
        // The fallback folds in what the CLI said, and the CLI's own failure
        // already reads as a sentence; repeating the forge around it reads
        // as "Could not reach GitLab (Could not reach GitLab (..))".
        let inner = GithubFailed::Network {
            forge: Forge::GitLab.title(),
            cause: "io: timeout".to_owned(),
        };
        assert_eq!(short_cause(&inner), "io: timeout");
        let http = GithubFailed::Http {
            forge: Forge::GitLab.title(),
            status: 403,
            what: "fetching the diff".to_owned(),
            hint: "Set GITLAB_TOKEN".to_owned(),
        };
        // No advice travels in a folded cause: the failure that ends the
        // attempt carries one, and two would read as two instructions.
        assert_eq!(short_cause(&http), "HTTP 403 while fetching the diff");
        // Folding keeps the kind of failure, so an HTTP status never comes back
        // as a connection problem with advice about cables.
        let said = [("glab", "it failed".to_owned())];
        let folded = said_so(http, &[], &said);
        match &folded {
            GithubFailed::Http { hint, .. } => {
                assert_eq!(hint, "Set GITLAB_TOKEN; glab said: it failed");
            }
            other => panic!("expected an HTTP failure, got {other}"),
        }
        // Two routes that both failed are both named, and the failure that
        // knows why leads.
        let folded = said_so(
            GithubFailed::Http {
                forge: Forge::GitLab.title(),
                status: 401,
                what: "fetching the diff".to_owned(),
                hint: "Check the token".to_owned(),
            },
            &[(
                "the page",
                GithubFailed::Http {
                    forge: Forge::GitLab.title(),
                    status: 404,
                    what: "fetching the diff".to_owned(),
                    hint: "".to_owned(),
                },
            )],
            &said,
        );
        assert_eq!(
            folded.to_string(),
            "GitLab returned HTTP 401 while fetching the diff. Check the token; the page said: HTTP 404 while fetching the diff; glab said: it failed"
        );
        // A failure with no room for a tail keeps what the other routes said
        // instead of dropping it: a `glab` failure must not turn into an
        // unrelated complaint about the page.
        let not_diff = GithubFailed::NotDiff {
            forge: Forge::GitLab.title(),
            url: "https://gitlab.com/a/b/-/merge_requests/1".to_owned(),
        };
        let folded = said_so(not_diff, &[], &said);
        assert!(matches!(folded, GithubFailed::Folded { .. }));
        assert!(folded.to_string().ends_with("glab said: it failed"));

        // Nothing folded means the failure is handed back untouched.
        assert_eq!(
            said_so(
                GithubFailed::Network {
                    forge: Forge::GitLab.title(),
                    cause: "io: timeout".to_owned(),
                },
                &[],
                &[],
            )
            .to_string(),
            "Could not reach GitLab (io: timeout). Check your connection and the link, then try again."
        );
        // Anything else keeps its own message.
        assert_eq!(
            short_cause(&GithubFailed::NotDiff {
                forge: Forge::GitLab.title(),
                url: "https://gitlab.com/a/b/-/merge_requests/1".to_owned(),
            }),
            "GitLab returned a page rather than a diff for https://gitlab.com/a/b/-/merge_requests/1. The project may be private, or the link may name something other than a merge request, a commit or a comparison."
        );
    }

    #[test]
    fn a_token_is_spent_on_the_api_path_of_each_page() {
        let mr = gitlab("code.example.com", pull("g/p", 7)).unwrap();
        assert_eq!(
            gitlab_api_request(&mr),
            (
                "https://code.example.com/api/v4/projects/g%2Fp/merge_requests/7/raw_diffs"
                    .to_owned(),
                "text/plain"
            )
        );
        let commit = gitlab(
            "code.example.com",
            Page::Commit {
                project: "g/p".into(),
                sha: "abcdef1".into(),
            },
        )
        .unwrap();
        assert_eq!(
            gitlab_api_request(&commit).0,
            "https://code.example.com/api/v4/projects/g%2Fp/repository/commits/abcdef1/diff"
        );
        // A ref keeps its slashes encoded, so a branch named `feature/x` is one
        // value and not two path segments.
        let compare = gitlab(
            "code.example.com",
            Page::Compare {
                project: "g/p".into(),
                base: "main".into(),
                head: "feature/x".into(),
            },
        )
        .unwrap();
        assert_eq!(
            gitlab_api_request(&compare).0,
            "https://code.example.com/api/v4/projects/g%2Fp/repository/compare?from=main&to=feature%2Fx"
        );
    }

    #[test]
    fn a_patch_is_rebuilt_from_the_changes_gitlab_lists() {
        // The API answers a commit with the list itself, a comparison under
        // `diffs`, and a merge request under `changes`.
        let drawn = r#"{"changes":[{"old_path":"a/x.txt","new_path":"a/x.txt","new_file":false,"deleted_file":false,"renamed_file":false,"a_mode":"100644","b_mode":"100644","diff":"@@ -1,2 +1,2 @@\n one\n-two\n+three"}]}"#;
        let changes = changes_of(drawn)
            .expect("a merge request's changes")
            .changes;
        let patch = patch_from_changes(&changes);
        assert_eq!(
            patch,
            "diff --git a/a/x.txt b/a/x.txt\n--- a/a/x.txt\n+++ b/a/x.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+three\n"
        );
        // Read as a diff, that patch is one file with one added and one removed
        // line, which is what makes the side-by-side view work.
        let parsed = crate::diff::Diff::parse(patch);
        assert_eq!(parsed.files().len(), 1);
        assert_eq!(parsed.files()[0].added, 1);
        assert_eq!(parsed.files()[0].removed, 1);

        let list = r#"[{"old_path":"gone.md","new_path":"gone.md","deleted_file":true,"a_mode":"100644","diff":"@@ -1,1 +0,0 @@\n-bye"},{"old_path":"new.md","new_path":"new.md","new_file":true,"b_mode":"100644","diff":"@@ -0,0 +1,1 @@\n+hello"}]"#;
        let patch = patch_from_changes(&changes_of(list).expect("a commit's diff").changes);
        assert!(patch.contains("diff --git a/gone.md b/gone.md\ndeleted file mode 100644\n--- a/gone.md\n+++ /dev/null\n"));
        assert!(patch.contains(
            "diff --git a/new.md b/new.md\nnew file mode 100644\n--- /dev/null\n+++ b/new.md\n"
        ));

        // A rename says so, and carries no hunks; a file GitLab sent nothing
        // for says that instead of looking unchanged.
        let renamed = r#"[{"old_path":"was.md","new_path":"is.md","renamed_file":true,"diff":""}]"#;
        let patch = patch_from_changes(&changes_of(renamed).expect("a renamed file").changes);
        assert_eq!(
            patch,
            "diff --git a/was.md b/is.md\nrename from was.md\nrename to is.md\n--- a/was.md\n+++ b/is.md\n"
        );
        let empty = r#"[{"old_path":"logo.png","new_path":"logo.png","diff":""}]"#;
        let patch = patch_from_changes(&changes_of(empty).expect("a file with no text").changes);
        assert!(patch.contains("leaf: GitLab sent no changes for this file"));

        // A path with a space is quoted the way git quotes one, or the header
        // would end the path at the space.
        let spaced =
            r#"[{"old_path":"a b/x.txt","new_path":"a b/x.txt","diff":"@@ -1,1 +1,1 @@\n-a\n+b"}]"#;
        let patch = patch_from_changes(&changes_of(spaced).expect("a path with a space").changes);
        assert!(patch.starts_with("diff --git \"a/a b/x.txt\" \"b/a b/x.txt\"\n"));
        assert_eq!(crate::diff::Diff::parse(patch).files().len(), 1);

        // A file made executable changes no lines, so its two modes are the
        // whole change and have to survive.
        let chmod = r#"[{"old_path":"run.sh","new_path":"run.sh","a_mode":"100644","b_mode":"100755","diff":""}]"#;
        let patch = patch_from_changes(&changes_of(chmod).expect("a chmod").changes);
        assert_eq!(
            patch,
            "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n--- a/run.sh\n+++ b/run.sh\n"
        );
        // A rename that also changes the mode says both.
        let both = r#"[{"old_path":"was.sh","new_path":"is.sh","a_mode":"100644","b_mode":"100755","renamed_file":true,"diff":""}]"#;
        let patch = patch_from_changes(&changes_of(both).expect("a rename and a chmod").changes);
        assert!(
            patch.contains(
                "old mode 100644\nnew mode 100755\nrename from was.sh\nrename to is.sh\n"
            )
        );

        // A page GitLab cuts short is refused rather than shown as whole: a
        // review that is missing files looks exactly like one that isn't.
        let capped = r#"{"changes":[],"overflow":true,"changes_count":"1000+"}"#;
        let changed = changes_of(capped).expect("an overflowed merge request");
        assert_eq!(
            changed.cut.as_deref(),
            Some("it sent 0 of at least 1000+ files")
        );
        let timed_out = r#"{"diffs":[],"compare_timeout":true}"#;
        assert_eq!(
            changes_of(timed_out)
                .expect("a comparison that timed out")
                .cut
                .as_deref(),
            Some("it sent 0 of more files than it sends")
        );
        let whole = r#"{"changes":[],"overflow":false,"changes_count":"3"}"#;
        assert!(changes_of(whole).expect("a whole page").cut.is_none());
        match patch_from_json(capped, "https://gitlab.com/a/b/-/merge_requests/1/changes") {
            Err(GithubFailed::Partial { detail, .. }) => {
                assert_eq!(detail, "it sent 0 of at least 1000+ files");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // An error page is not a list of changes, however it arrived.
        assert!(changes_of("<html>Not Found</html>").is_none());
        assert!(changes_of(r#"{"message":"404 Ref Not Found"}"#).is_none());
    }

    #[test]
    fn glab_is_asked_only_where_it_can_answer() {
        let none: &[String] = &[];
        let mr = gitlab("gitlab.com", pull("a/b", 12)).unwrap();
        assert_eq!(
            glab_command(&mr, true, none),
            Some(
                [
                    "mr",
                    "diff",
                    "12",
                    "--repo",
                    "https://gitlab.com/a/b",
                    "--raw",
                    "--color",
                    "never"
                ]
                .map(str::to_owned)
                .to_vec()
            )
        );
        // A self-hosted merge request is asked on its own host, never on
        // whichever one `glab` defaults to.
        let configured = parse_hosts("code.example.com");
        let hosted = gitlab("code.example.com", pull("g/p", 7)).unwrap();
        let args = glab_command(&hosted, true, &configured).expect("a named instance");
        assert_eq!(args[3..5], ["--repo", "https://code.example.com/g/p"]);
        // Nothing runs when the CLI is absent, when the page has no `glab`
        // command of its own, or when the host isn't one leaf trusts.
        assert_eq!(glab_command(&mr, false, none), None);
        let commit = gitlab(
            "gitlab.com",
            Page::Commit {
                project: "a/b".into(),
                sha: "abcdef1".into(),
            },
        )
        .unwrap();
        assert_eq!(glab_command(&commit, true, none), None);
        let compare = gitlab(
            "gitlab.com",
            Page::Compare {
                project: "a/b".into(),
                base: "v1".into(),
                head: "v2".into(),
            },
        )
        .unwrap();
        assert_eq!(glab_command(&compare, true, none), None);
        assert_eq!(
            glab_command(
                &gitlab("code.example.com", pull("g/p", 7)).unwrap(),
                true,
                none
            ),
            None
        );
    }

    #[test]
    fn a_self_hosted_instance_is_named_by_its_host_or_by_gitlab_host() {
        let none: &[String] = &[];
        assert_eq!(forge_of_hosts("github.com", none), Some(Forge::GitHub));
        assert_eq!(forge_of_hosts("gitlab.com", none), Some(Forge::GitLab));
        assert_eq!(
            forge_of_hosts("gitlab.example.com", none),
            Some(Forge::GitLab)
        );
        // A name that says nothing is a GitLab once GITLAB_HOST names it, and
        // that is the only host its token goes to.
        assert_eq!(forge_of_hosts("code.example.com", none), None);
        let configured = parse_hosts("code.example.com, https://git2.example.com:8443/");
        assert_eq!(configured, ["code.example.com", "git2.example.com"]);
        assert!(is_gitlab_host("code.example.com", &configured));
        assert!(is_gitlab_host("Code.Example.com:8443", &configured));
        assert!(is_gitlab_host("git2.example.com", &configured));
        assert_eq!(
            forge_of_hosts("code.example.com", &configured),
            Some(Forge::GitLab)
        );
        assert_eq!(
            forge_of_hosts("Code.Example.com", &configured),
            Some(Forge::GitLab)
        );
        // A host that only shares a prefix, or isn't named at all, gets nothing.
        assert!(!is_gitlab_host("code.example.com.evil.test", &configured));
        assert!(!is_gitlab_host("other.example.com", &configured));
        assert!(!is_gitlab_host("", &configured));
        // Naming GitHub as a GitLab by mistake must not send a token there.
        let oops = parse_hosts("github.com");
        assert!(!is_gitlab_host("github.com", &oops));
        assert_eq!(forge_of_hosts("github.com", &oops), Some(Forge::GitHub));
        assert_eq!(
            forge_of_hosts("code.example.com.evil.test", &configured),
            None
        );
    }

    #[test]
    fn gitlab_labels_urls_and_keys_name_their_host() {
        let pull = Resource {
            forge: Forge::GitLab,
            host: "gitlab.com".into(),
            page: Page::Pull {
                project: "a/b/c".into(),
                number: 12,
            },
        };
        assert_eq!(pull.label(), "a/b/c!12");
        assert_eq!(
            pull.diff_url(),
            "https://gitlab.com/a/b/c/-/merge_requests/12.diff"
        );
        // Two hosts can serve one project, and a merge request and a pull
        // request can share a number, so neither shares a saved diff.
        assert_eq!(pull.key(), "gitlab.com/a/b/c/-/merge_requests/12");
        let elsewhere = Resource {
            host: "git.example.com".into(),
            ..pull.clone()
        };
        assert_ne!(elsewhere.key(), pull.key());
        let commit = Resource {
            forge: Forge::GitLab,
            host: "gitlab.com".into(),
            page: Page::Commit {
                project: "o/r".into(),
                sha: "0123456789abcdef".into(),
            },
        };
        assert_eq!(
            commit.diff_url(),
            "https://gitlab.com/o/r/-/commit/0123456789abcdef.diff"
        );
    }

    #[test]
    fn hosts_name_their_forge() {
        assert_eq!(Forge::of_host("github.com"), Some(Forge::GitHub));
        assert_eq!(Forge::of_host("GitLab.com"), Some(Forge::GitLab));
        assert_eq!(Forge::of_host("gitlab.example.com"), Some(Forge::GitLab));
        assert_eq!(
            Forge::of_host("gitlab.example.com:8443"),
            Some(Forge::GitLab)
        );
        assert_eq!(Forge::of_host("code.example.com"), None);
    }

    #[test]
    fn pull_metadata_yields_the_head_sha() {
        let json = r#"{"url":"https://github.com/o/r/pull/2","head":{"ref":"feat","sha":"abc1234def"},"base":{"sha":"0000000"}}"#;
        assert_eq!(json_head_sha(json).as_deref(), Some("abc1234def"));
        let pulls = r#"[{"number":42,"title":"x"},{"number":7}]"#;
        assert_eq!(json_number_after(pulls, "number"), Some(42));
        assert_eq!(json_number_after("[]", "number"), None);
    }

    #[test]
    fn pull_head_sha_does_not_read_other_objects() {
        for json in [
            r#"{"head":null,"base":{"sha":"base"}}"#,
            r#"{"head":{},"base":{"sha":"base"}}"#,
            r#"{"base":{"sha":"base"}}"#,
            r#"{"head":{"sha":42}}"#,
            r#"{"head":{"sha":"truncated"}"#,
        ] {
            assert_eq!(json_head_sha(json), None, "{json}");
        }
        let json = r#"{"head":{"repo":{"sha":"nested"},"sha":"correct"}}"#;
        assert_eq!(json_head_sha(json).as_deref(), Some("correct"));
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_escape("a\"b\nc\\d"), "a\\\"b\\nc\\\\d");
        assert_eq!(json_escape("tab\there"), "tab\\there");
    }
}
