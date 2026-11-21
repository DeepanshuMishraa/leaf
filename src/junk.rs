//! What a folder on disk is, for `leaf storage`: which part of the disk it
//! counts toward, and whether it's junk that rebuilds itself, scratch an
//! agent left behind, or someone's data. Only the path and a marker file or
//! two are looked at, so this runs on every folder of a scan.

use std::path::Path;

/// The parts of a disk the storage screen shows, in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    Apps,
    Developer,
    Agents,
    Media,
    Documents,
    Caches,
    Trash,
    Other,
    System,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::Apps,
        Category::Developer,
        Category::Agents,
        Category::Media,
        Category::Documents,
        Category::Caches,
        Category::Trash,
        Category::Other,
        Category::System,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            Category::Apps => "Apps",
            Category::Developer => "Developer",
            Category::Agents => "AI agents",
            Category::Media => "Media",
            Category::Documents => "Documents",
            Category::Caches => "Caches",
            Category::Trash => "Trash",
            Category::Other => "Other files",
            Category::System => "System data",
        }
    }

    /// One sentence on what's in it.
    pub fn about(self) -> &'static str {
        match self {
            Category::Apps => "Applications and the data they keep for themselves.",
            Category::Developer => {
                "Build output, dependencies, package caches, SDKs and simulators."
            }
            Category::Agents => {
                "Sessions, logs, models and scratch files left by AI agents and their tools."
            }
            Category::Media => "Photos, movies, music and message attachments.",
            Category::Documents => "Documents, the desktop, downloads and cloud folders.",
            Category::Caches => "Apps and tools rebuild these when they need them.",
            Category::Trash => "Already deleted. Emptying it frees the space.",
            Category::Other => "Everything else in your folders.",
            Category::System => {
                "The system, its settings and logs, and anything leaf can't read: snapshots, swap and other accounts' files."
            }
        }
    }
}

/// How sure leaf is that something can go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Rebuilt or downloaded again on its own when needed.
    Junk,
    /// Scratch files: junk once nothing has touched them for a while.
    Temp,
    /// Someone's data. Safe to delete only if it's no longer wanted.
    Review,
}

/// What leaf knows about a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    pub label: &'static str,
    pub verdict: Verdict,
    pub why: &'static str,
}

const fn junk(label: &'static str, why: &'static str) -> Finding {
    Finding {
        label,
        verdict: Verdict::Junk,
        why,
    }
}

const fn review(label: &'static str, why: &'static str) -> Finding {
    Finding {
        label,
        verdict: Verdict::Review,
        why,
    }
}

const fn temp(label: &'static str, why: &'static str) -> Finding {
    Finding {
        label,
        verdict: Verdict::Temp,
        why,
    }
}

const APP_CACHE: Finding = junk("cache", "apps rebuild it when they need it");
const APP_DATA: Finding = review(
    "app data",
    "settings and files an app keeps; removed with the app",
);
const LOGS: Finding = junk("logs", "old logs; programs start new ones");
const TRASH: Finding = junk("trash", "already deleted; this frees the space");
const TEMP: Finding = temp(
    "temporary files",
    "scratch space; the system clears it on restart",
);
const SYSTEM_CACHE: Finding = junk("system cache", "rebuilt by the package manager");
const DOWNLOAD: Finding = review("download", "downloaded once; often safe to remove");
const CLOUD: Finding = review(
    "cloud folder",
    "synced with a cloud service; deleting it here deletes it there",
);

const NODE_MODULES: Finding = junk("node_modules", "npm, pnpm, yarn or bun install restores it");
const TOOL_MODULES: Finding = review(
    "tool dependencies",
    "installed by a tool in its own folder; it may need reinstalling",
);
const CARGO_TARGET: Finding = junk("cargo build output", "cargo build rebuilds it");
const BUILD_CACHE: Finding = junk("build cache", "the next build recreates it");
const DERIVED: Finding = junk("Xcode DerivedData", "Xcode rebuilds it on the next build");
const DEVICE_SUPPORT: Finding = junk(
    "Xcode device support",
    "Xcode copies it again when that device connects",
);
const ARCHIVES: Finding = review(
    "Xcode archives",
    "builds you archived; keep the ones you may need to symbolicate",
);
const SIM_CACHE: Finding = junk("simulator caches", "the simulator rebuilds them");
const SIMULATORS: Finding = review(
    "iOS simulators",
    "simulator devices and their apps; `xcrun simctl delete unavailable` removes old ones",
);
const VENV: Finding = junk(
    "Python virtualenv",
    "recreate it from the project's requirements",
);
const PODS: Finding = junk("CocoaPods", "pod install restores it");
const GRADLE_PROJECT: Finding = junk("Gradle project cache", "Gradle recreates it");
const TAGGED_CACHE: Finding = junk("cache", "marked as a cache by the program that made it");
const NPM_CACHE: Finding = junk("npm cache", "npm downloads packages again when needed");
const NPX: Finding = junk("npx cache", "npx downloads the tool again when run");
const BUN_CACHE: Finding = junk("bun cache", "bun downloads packages again when needed");
const PNPM: Finding = junk(
    "pnpm store",
    "pnpm downloads packages again; `pnpm store prune` trims it",
);
const YARN: Finding = junk("yarn cache", "yarn downloads packages again when needed");
const CARGO_REGISTRY: Finding = junk("cargo registry", "cargo downloads crates again when needed");
const CARGO_GIT: Finding = junk(
    "cargo git checkouts",
    "cargo fetches them again when needed",
);
const TOOLCHAIN: Finding = review(
    "Rust toolchain",
    "an installed toolchain; `rustup toolchain list` shows which you use",
);
const GO_MOD: Finding = junk(
    "Go module cache",
    "go downloads modules again; `go clean -modcache`",
);
const GRADLE: Finding = junk("Gradle cache", "Gradle downloads dependencies again");
const GRADLE_WRAPPER: Finding = junk("Gradle distributions", "the wrapper downloads Gradle again");
const MAVEN: Finding = junk("Maven repository", "Maven downloads dependencies again");
const COCOAPODS: Finding = junk(
    "CocoaPods cache",
    "CocoaPods downloads specs and pods again",
);
const NUGET: Finding = junk("NuGet packages", "dotnet restore downloads them again");
const PUB: Finding = junk("Dart pub cache", "pub get downloads packages again");
const HOMEBREW: Finding = junk("Homebrew downloads", "brew cleanup removes them too");
const PLAYWRIGHT: Finding = junk(
    "Playwright browsers",
    "playwright install downloads them again",
);
const ANDROID_SDK: Finding = review("Android SDK", "remove unused platforms in Android Studio");
const AVD: Finding = review("Android emulators", "emulator images and their disks");
const DOCKER: Finding = review(
    "Docker disk image",
    "images, containers and volumes; `docker system prune` trims it",
);
const EXTENSIONS: Finding = review("editor extensions", "installed extensions");
const ICLOUD: Finding = review(
    "iCloud Drive",
    "synced with iCloud; deleting it here deletes it there",
);
const MAIL: Finding = review("Mail", "your mail and attachments");
const MESSAGES: Finding = review("Messages", "message history and attachments");
const SPOTLIGHT: Finding = review("drive index", "Spotlight and file system bookkeeping");

const CLAUDE_SESSIONS: Finding = review(
    "Claude Code sessions",
    "transcripts that `claude --resume` reopens",
);
const CLAUDE_HISTORY: Finding = review(
    "Claude Code checkpoints",
    "file snapshots for rewinding edits in past sessions",
);
const CLAUDE_DESKTOP: Finding = review(
    "Claude app data",
    "the Claude app's data, including its sandbox images",
);
const CODEX_SESSIONS: Finding = review("Codex sessions", "transcripts that `codex resume` reopens");
const AGENT_LOGS: Finding = junk("agent logs", "debug logs; the agent starts new ones");
const AGENT_JUNK: Finding = junk("agent scratch", "per-session state the agent recreates");
const AGENT_TEMP: Finding = temp(
    "agent scratch files",
    "temporary files an agent wrote while working",
);
const AGENT_RUNTIME: Finding = junk("agent runtime", "downloaded again when the agent needs it");
const OLLAMA: Finding = review("Ollama models", "local models; `ollama rm` removes one");
const LMSTUDIO: Finding = review("LM Studio models", "downloaded models");
const HUGGINGFACE: Finding = review(
    "Hugging Face models",
    "models and datasets; downloaded again when used",
);
const MODEL_CACHE: Finding = review("model cache", "downloaded models; fetched again when used");
const OPENCODE: Finding = review("opencode data", "sessions and state");

/// What a place on disk is, and how its contents are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// One thing: listed, sized and cleared as a whole.
    Item {
        category: Category,
        finding: Option<Finding>,
    },
    /// Each thing directly inside is listed on its own, with `finding`.
    Container {
        category: Category,
        finding: Option<Finding>,
    },
    /// Counts toward `category`; with `list`, each folder directly in it is
    /// listed too, and the search for items goes on inside.
    Area { category: Category, list: bool },
}

const fn item(category: Category, finding: Finding) -> Option<Role> {
    Some(Role::Item {
        category,
        finding: Some(finding),
    })
}

const fn container(category: Category, finding: Option<Finding>) -> Option<Role> {
    Some(Role::Container { category, finding })
}

const fn area(category: Category) -> Option<Role> {
    Some(Role::Area {
        category,
        list: true,
    })
}

/// A folder being looked at.
#[derive(Debug, Clone, Copy)]
pub struct Spot<'a> {
    pub path: &'a Path,
    pub name: &'a str,
    /// Its path below the home folder, `/`-separated, when it's in there.
    pub rel: Option<&'a str>,
    /// Whether it's directly in a temporary folder.
    pub temp: bool,
}

/// What `spot` is, or `None` when it's just more of what it's in.
pub fn classify(spot: &Spot) -> Option<Role> {
    use Category::*;
    if let Some(rel) = spot.rel
        && let Some(role) = home_rule(rel)
    {
        return Some(role);
    }
    if spot.rel.is_none()
        && let Some(role) = spot.path.to_str().and_then(system_rule)
    {
        return Some(role);
    }
    if spot.temp && is_agent_name(spot.name) {
        return item(Agents, AGENT_TEMP);
    }
    let marked = |file: &str| spot.path.join(file).exists();
    match spot.name {
        "node_modules" if is_project_dependencies(spot.path) => item(
            Developer,
            if in_tool_folder(spot.rel) {
                TOOL_MODULES
            } else {
                NODE_MODULES
            },
        ),
        "target" if marked("CACHEDIR.TAG") || marked(".rustc_info.json") => {
            item(Developer, CARGO_TARGET)
        }
        ".next" | ".nuxt" | ".svelte-kit" | ".turbo" | ".parcel-cache" | ".angular" | ".expo"
        | ".docusaurus" | ".dart_tool" | "zig-cache" | ".zig-cache" | ".stack-work"
        | "dist-newstyle" | ".terraform" => item(Developer, BUILD_CACHE),
        "DerivedData" => item(Developer, DERIVED),
        ".venv" | "venv" | "env" if marked("pyvenv.cfg") => item(Developer, VENV),
        "Pods" if spot.path.with_file_name("Podfile").exists() => item(Developer, PODS),
        ".gradle" => item(Developer, GRADLE_PROJECT),
        ".Trashes" => item(Trash, TRASH),
        name if name.starts_with(".Trash-") => item(Trash, TRASH),
        ".Spotlight-V100" | ".fseventsd" | ".DocumentRevisions-V100" => item(System, SPOTLIGHT),
        // Only folders named like a cache are checked: a stat per folder adds up.
        name if sounds_like_cache(name) && marked("CACHEDIR.TAG") => item(Caches, TAGGED_CACHE),
        _ => None,
    }
}

/// Places with a fixed meaning outside the home folder.
fn system_rule(path: &str) -> Option<Role> {
    use Category::*;
    match path {
        "/Library/Caches" => container(Caches, Some(APP_CACHE)),
        "/Library/Logs" => container(System, Some(LOGS)),
        "/Library/Application Support" => container(Apps, Some(APP_DATA)),
        "/Library/Developer" => area(Developer),
        "/opt/homebrew/Caskroom" | "/opt/homebrew/Cellar" => container(Developer, None),
        _ => None,
    }
}

/// Whether a node_modules folder is a project's own, which an install
/// restores, rather than where global tools live.
fn is_project_dependencies(path: &Path) -> bool {
    let Some(project) = path.parent() else {
        return false;
    };
    // A package's own dependencies, inside an installed package or a
    // global prefix: removing them breaks the tool.
    let installed = project.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("node_modules" | ".npm-global" | ".nvm" | ".volta" | ".fnm")
        )
    });
    let global = project
        .file_name()
        .is_some_and(|name| name == "global" || name == "lib");
    !installed && !global && project.join("package.json").exists()
}

/// Whether a project sits inside a tool's hidden folder in home, like
/// `~/.pi/agent`, where the tool installed it rather than you.
fn in_tool_folder(rel: Option<&str>) -> bool {
    rel.is_some_and(|rel| rel.starts_with('.'))
}

/// Places with a fixed meaning in the home folder.
fn home_rule(rel: &str) -> Option<Role> {
    use Category::*;
    // Children of containers that deserve their own label.
    if let Some(name) = rel.strip_prefix("Library/Caches/") {
        return cache_child(name);
    }
    if let Some(name) = rel.strip_prefix(".cache/") {
        return cache_child(name);
    }
    match rel {
        "Library" => area(System),
        "Library/Caches" | ".cache" => container(Caches, Some(APP_CACHE)),
        "Library/Logs" => container(System, Some(LOGS)),
        "Library/Application Support"
        | "Library/Containers"
        | "Library/Group Containers"
        | ".config" => container(Apps, Some(APP_DATA)),
        "Library/Application Support/Claude" => item(Agents, CLAUDE_DESKTOP),
        "Library/Containers/com.docker.docker" => item(Developer, DOCKER),
        "Library/Developer" | "Library/Developer/Xcode" | "Library/Developer/CoreSimulator" => {
            area(Developer)
        }
        "Library/Developer/Xcode/DerivedData" => item(Developer, DERIVED),
        "Library/Developer/Xcode/iOS DeviceSupport"
        | "Library/Developer/Xcode/watchOS DeviceSupport"
        | "Library/Developer/Xcode/tvOS DeviceSupport"
        | "Library/Developer/Xcode/visionOS DeviceSupport" => item(Developer, DEVICE_SUPPORT),
        "Library/Developer/Xcode/Archives" => item(Developer, ARCHIVES),
        "Library/Developer/CoreSimulator/Caches" => item(Developer, SIM_CACHE),
        "Library/Developer/CoreSimulator/Devices" => item(Developer, SIMULATORS),
        "Library/Mobile Documents" => item(Documents, ICLOUD),
        "Library/CloudStorage" => container(Documents, Some(CLOUD)),
        "Library/Mail" => item(Documents, MAIL),
        "Library/Messages" => item(Media, MESSAGES),
        "Library/Android/sdk" => item(Developer, ANDROID_SDK),
        "Library/pnpm/store" | ".pnpm-store" | ".local/share/pnpm/store" => item(Developer, PNPM),
        ".Trash" => container(Trash, Some(TRASH)),
        ".local/share/Trash" => item(Trash, TRASH),
        "Applications" => container(Apps, None),
        "Downloads" => container(Documents, Some(DOWNLOAD)),
        "Documents" | "Desktop" => container(Documents, None),
        ".local" => area(Other),
        ".local/share" => container(Apps, Some(APP_DATA)),
        "Pictures" | "Movies" | "Music" | "Videos" => container(Media, None),

        ".npm" | ".bun" | ".cargo" | ".rustup" | "go" | "go/pkg" | ".gradle" | ".m2"
        | ".android" | ".docker" | ".yarn" | ".yarn/berry" | ".bun/install" => area(Developer),
        ".npm/_cacache" => item(Developer, NPM_CACHE),
        ".npm/_npx" => item(Developer, NPX),
        ".npm/_logs" => item(Developer, LOGS),
        ".bun/install/cache" => item(Developer, BUN_CACHE),
        ".yarn/berry/cache" => item(Developer, YARN),
        ".cargo/registry" => item(Developer, CARGO_REGISTRY),
        ".cargo/git" => item(Developer, CARGO_GIT),
        ".rustup/toolchains" => container(Developer, Some(TOOLCHAIN)),
        "go/pkg/mod" => item(Developer, GO_MOD),
        ".gradle/caches" => item(Developer, GRADLE),
        ".gradle/wrapper/dists" => item(Developer, GRADLE_WRAPPER),
        ".m2/repository" => item(Developer, MAVEN),
        ".cocoapods" => item(Developer, COCOAPODS),
        ".nuget/packages" => item(Developer, NUGET),
        ".pub-cache" => item(Developer, PUB),
        ".android/avd" => item(Developer, AVD),
        ".vscode/extensions" => item(Developer, EXTENSIONS),

        ".claude" | ".codex" | ".gemini" | ".cursor" | ".ollama" | ".lmstudio" | ".factory"
        | ".qwen" | ".continue" | ".codeium" | ".windsurf" | ".copilot" | ".kiro" | ".goose"
        | ".aider" | ".amp" | ".pi" | ".crush" | ".opencode" | ".t3" => area(Agents),
        ".claude/projects" => item(Agents, CLAUDE_SESSIONS),
        ".claude/file-history" => item(Agents, CLAUDE_HISTORY),
        ".claude/debug" | ".codex/log" | ".gemini/logs" => item(Agents, AGENT_LOGS),
        ".claude/shell-snapshots" | ".claude/todos" | ".claude/statsig" => item(Agents, AGENT_JUNK),
        ".codex/sessions" | ".codex/archived_sessions" => item(Agents, CODEX_SESSIONS),
        ".gemini/tmp" | ".codex/.tmp" => item(Agents, AGENT_TEMP),
        ".cursor/extensions" | ".windsurf/extensions" => item(Agents, EXTENSIONS),
        ".ollama/models" => item(Agents, OLLAMA),
        ".lmstudio/models" => item(Agents, LMSTUDIO),
        ".local/share/opencode" => item(Agents, OPENCODE),
        _ => None,
    }
}

/// A folder directly in a cache folder that's better known than "cache".
fn cache_child(name: &str) -> Option<Role> {
    use Category::*;
    let cache = |finding| item(Caches, finding);
    match name {
        "huggingface" => item(Agents, HUGGINGFACE),
        "torch" | "whisper" | "lm-studio" | "llama.cpp" | "ollama" => item(Agents, MODEL_CACHE),
        "codex-runtimes" => item(Agents, AGENT_RUNTIME),
        "Homebrew" => cache(HOMEBREW),
        "ms-playwright" => cache(PLAYWRIGHT),
        "pip" | "uv" | "pypoetry" => {
            cache(junk("Python packages", "downloaded again when installing"))
        }
        "Yarn" | "yarn" => cache(YARN),
        "pnpm" => cache(PNPM),
        "go-build" => cache(junk("Go build cache", "go build recreates it")),
        "bazel" => cache(junk("Bazel cache", "Bazel rebuilds it")),
        "CocoaPods" => cache(COCOAPODS),
        "JetBrains" => cache(junk("JetBrains caches", "the IDE reindexes on start")),
        _ => None,
    }
}

/// A folder named for an AI agent, as agents name their scratch folders in /tmp.
fn is_agent_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "claude", "codex", "cursor", "gemini", "opencode", "aider", "copilot", "ollama", "amp",
        "goose", "crush", "windsurf", "qwen", "factory", "droid",
    ]
    .iter()
    .any(|agent| name.starts_with(agent))
}

/// What a folder at the top of a scan is: the home folder, apps, temporary
/// files, system caches, or a whole drive.
pub fn root_role(path: &Path, home: Option<&Path>, temps: &[&Path]) -> Role {
    use Category::*;
    if home == Some(path) {
        return Role::Area {
            category: Other,
            list: true,
        };
    }
    if temps.contains(&path) {
        return Role::Container {
            category: Caches,
            finding: Some(TEMP),
        };
    }
    match path.to_str() {
        Some("/Library") => Role::Area {
            category: System,
            list: true,
        },
        Some("/opt/homebrew" | "/usr/local") => Role::Area {
            category: Developer,
            list: true,
        },
        Some("/Applications" | "/opt") => Role::Container {
            category: Apps,
            finding: None,
        },
        Some("/var/cache") => Role::Container {
            category: Caches,
            finding: Some(SYSTEM_CACHE),
        },
        Some("/var/log") => Role::Container {
            category: System,
            finding: Some(LOGS),
        },
        _ => Role::Area {
            category: Other,
            list: true,
        },
    }
}

/// Whether a folder's name says it holds a cache, for the folder most of
/// an item's space is in.
pub fn sounds_like_cache(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("cache")
        || matches!(
            name,
            "Cache_Data" | "GPUCache" | "Code Cache" | "Service Worker" | "tmp" | "temp"
        )
}

/// How something found reads in the list, once its age is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Safe to clear.
    Junk,
    /// Not touched in months.
    Unused,
    /// Scratch still being written.
    InUse,
    /// Data to look at before deleting.
    Review,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Junk => "junk",
            Status::Unused => "unused",
            Status::InUse => "in use",
            Status::Review => "review",
        }
    }

    /// Whether `f` (junk and unused only) keeps it.
    pub fn reclaimable(self) -> bool {
        matches!(self, Status::Junk | Status::Unused)
    }
}

/// Days without a change after which data reads as unused.
const UNUSED_DAYS: u64 = 180;
/// Days without a change after which scratch files read as junk.
const STALE_SCRATCH_DAYS: u64 = 2;

/// How something with `finding`, last changed `age_days` ago, reads.
pub fn status(finding: Option<&Finding>, age_days: Option<u64>) -> Option<Status> {
    let old = |days| age_days.is_some_and(|age| age >= days);
    match finding.map(|f| f.verdict) {
        Some(Verdict::Junk) => Some(Status::Junk),
        Some(Verdict::Temp) if old(STALE_SCRATCH_DAYS) => Some(Status::Junk),
        Some(Verdict::Temp) => Some(Status::InUse),
        Some(Verdict::Review) if old(UNUSED_DAYS) => Some(Status::Unused),
        Some(Verdict::Review) => Some(Status::Review),
        None if old(UNUSED_DAYS * 2) => Some(Status::Unused),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spot<'a>(path: &'a Path, rel: Option<&'a str>) -> Spot<'a> {
        Spot {
            path,
            name: path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            rel,
            temp: false,
        }
    }

    fn category(role: Option<Role>) -> Option<Category> {
        role.map(|role| match role {
            Role::Item { category, .. }
            | Role::Container { category, .. }
            | Role::Area { category, .. } => category,
        })
    }

    #[test]
    fn home_places_land_in_their_categories() {
        let at = |rel: &str| {
            let path = Path::new("/home/a").join(rel);
            category(classify(&spot(&path, Some(rel))))
        };
        assert_eq!(at("Library/Caches"), Some(Category::Caches));
        assert_eq!(at(".cache/huggingface"), Some(Category::Agents));
        assert_eq!(at(".claude/projects"), Some(Category::Agents));
        assert_eq!(at(".cargo/registry"), Some(Category::Developer));
        assert_eq!(at(".Trash"), Some(Category::Trash));
        assert_eq!(
            at("Library/Developer/Xcode/DerivedData"),
            Some(Category::Developer)
        );
        assert_eq!(at("Notes"), None);
    }

    #[test]
    fn caches_are_containers_and_their_children_items() {
        let path = Path::new("/home/a/Library/Caches");
        assert!(matches!(
            classify(&spot(path, Some("Library/Caches"))),
            Some(Role::Container { finding: Some(f), .. }) if f.verdict == Verdict::Junk
        ));
        let path = Path::new("/home/a/Library/Caches/Homebrew");
        assert!(matches!(
            classify(&spot(path, Some("Library/Caches/Homebrew"))),
            Some(Role::Item { finding: Some(f), .. }) if f.label == "Homebrew downloads"
        ));
    }

    #[test]
    fn build_output_is_found_anywhere_by_name_and_marker() {
        let dir = std::env::temp_dir().join(format!("leaf-junk-{}", std::process::id()));
        let target = dir.join("target");
        std::fs::create_dir_all(&target).unwrap();
        assert_eq!(
            classify(&spot(&target, None)),
            None,
            "no marker, no verdict"
        );
        std::fs::write(target.join("CACHEDIR.TAG"), "Signature").unwrap();
        assert_eq!(
            classify(&spot(&target, None)),
            item(Category::Developer, CARGO_TARGET)
        );
        let modules = dir.join("node_modules");
        assert_eq!(
            classify(&spot(&modules, None)),
            None,
            "no package.json, no project"
        );
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        assert_eq!(
            classify(&spot(&modules, None)),
            item(Category::Developer, NODE_MODULES)
        );
        let global = dir.join("global");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("package.json"), "{}").unwrap();
        assert_eq!(
            classify(&spot(&global.join("node_modules"), None)),
            None,
            "global tools aren't junk"
        );
        let nested = modules.join("@openai/codex");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("package.json"), "{}").unwrap();
        assert_eq!(
            classify(&spot(&nested.join("node_modules"), None)),
            None,
            "an installed package's dependencies aren't junk"
        );
        assert_eq!(
            classify(&spot(&modules, Some(".pi/agent/node_modules"))),
            item(Category::Developer, TOOL_MODULES),
            "a tool's own install is left for review"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn agent_scratch_in_temp_is_junk_once_left_alone() {
        let path = Path::new("/tmp/claude-501");
        let role = classify(&Spot {
            temp: true,
            ..spot(path, None)
        });
        let Some(Role::Item {
            category: Category::Agents,
            finding: Some(finding),
        }) = role
        else {
            panic!("{role:?}");
        };
        assert_eq!(status(Some(&finding), Some(0)), Some(Status::InUse));
        assert_eq!(status(Some(&finding), Some(5)), Some(Status::Junk));
        assert_eq!(classify(&spot(path, None)), None, "only in temp folders");
    }

    #[test]
    fn data_reads_as_unused_after_months() {
        assert_eq!(status(Some(&DOWNLOAD), Some(10)), Some(Status::Review));
        assert_eq!(status(Some(&DOWNLOAD), Some(400)), Some(Status::Unused));
        assert_eq!(status(Some(&NPM_CACHE), Some(0)), Some(Status::Junk));
        assert_eq!(status(None, Some(30)), None);
        assert_eq!(status(None, None), None);
    }

    #[test]
    fn roots_say_what_they_hold() {
        let home = Path::new("/home/a");
        let tmp = Path::new("/tmp");
        assert!(matches!(
            root_role(home, Some(home), &[tmp]),
            Role::Area {
                category: Category::Other,
                list: true
            }
        ));
        assert!(matches!(
            root_role(tmp, Some(home), &[tmp]),
            Role::Container {
                category: Category::Caches,
                ..
            }
        ));
        assert!(matches!(
            root_role(Path::new("/Applications"), Some(home), &[]),
            Role::Container {
                category: Category::Apps,
                finding: None
            }
        ));
    }

    #[test]
    fn cache_folders_are_recognised_by_name() {
        assert!(sounds_like_cache("Cache_Data"));
        assert!(sounds_like_cache("GPUCache"));
        assert!(sounds_like_cache("_cacache"));
        assert!(!sounds_like_cache("Documents"));
    }
}
