//! `leaf sim`: an installed iOS Simulator on a 3D iPhone in a Rex pane, on
//! macOS, and the commands an agent drives it with.
//!
//! The pane is a native helper (src/simulator.swift and src/pane.swift,
//! compiled by build.rs and embedded here; see src/helper.rs), unpacked with
//! the iPhone and iPhone Duo models beside it. It boots the device, shows its
//! live screen and sends touches and keys. It also listens on a Unix socket, one per
//! device, which `leaf sim snapshot|tap|swipe|type|key|button|info` talk to.
//! With no pane on a booted device, those run the helper headless
//! (`leaf-sim-host --drive`), one request each. `screenshot`, `open-url`
//! and `launch` go through `simctl`. Snapshots and their refs are in
//! simsnap.rs.
//!
//! Asking about the screen: the helper writes one JSON line per question to
//! its stdout; this process saves a screenshot, sends the question with its
//! path to the person's agent and answers on the helper's stdin.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::helper::{Part, macos_version};
use crate::simsnap::{Detail, Element, Snapshot, Tree, parse_ref};

const SIM_HOST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/leaf-sim-host"));
/// Runs inside the iPhone Duo's simulator to move its hinge (src/duo-hinge.m).
const DUO_HINGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/leaf-duo-hinge"));

const USAGE: &str = "Usage: leaf sim install [--yes]          download the 3D iPhones and turn the simulator on (asks first)
       leaf sim list [--json]            installed iPhone simulators
       leaf sim open <udid>              show one in this Rex pane, on a 3D iPhone
       leaf sim info                     the device and its screen size in pixels
       leaf sim snapshot [-i] [--diff]   the screen's elements, each with a ref (@e3) to act on;
                                         -i only what can be acted on, --diff only what changed
       leaf sim screenshot [file.png]    save the screen; prints the file's path
       leaf sim tap <@ref | x y>         tap an element, or a point in screenshot pixels (0,0 top left)
       leaf sim swipe <x1> <y1> <x2> <y2> [seconds]
       leaf sim scroll <up|down|left|right> [@ref]
       leaf sim type [@ref] <text>       type on the simulator's keyboard, first tapping @ref
       leaf sim fill <@ref> <text>       tap a field, clear it and type
       leaf sim key <name>               enter, escape, backspace, tab, space, up, down… or a chord: cmd+a
       leaf sim button <name>            home, lock, side or siri
       leaf sim wait <text> [seconds]    until an element showing the text is on screen (10 s)
       leaf sim apps                     installed apps and their bundle ids
       leaf sim launch <bundle id|name>  start an installed app
       leaf sim open-url <url>           open a link or deep link
Add --device <udid> to pick a simulator when more than one is open or booted.";

/// One installed iPhone simulator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    pub udid: String,
    pub name: String,
    /// Such as "iOS 26.1".
    pub runtime: String,
    /// simctl's state: "Booted", "Shutdown", "Booting"…
    pub state: String,
    /// The 3D body the pane draws it on.
    #[serde(skip)]
    body: Body,
    #[serde(skip)]
    version: Vec<u32>,
}

/// Which 3D phone the pane draws a simulator on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    /// The foldable iPhone Duo, which opens and shuts.
    Duo,
    Air,
    Pro17,
    Iphone16,
    /// Every other iPhone.
    Generic,
}

impl Body {
    /// The body for a simctl device type identifier, such as
    /// `com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro`.
    fn for_device_type(identifier: &str) -> Body {
        match identifier.rsplit('.').next().unwrap_or_default() {
            "iPhone-Duo" => Body::Duo,
            "iPhone-Air" => Body::Air,
            "iPhone-17-Pro" | "iPhone-17-Pro-Max" => Body::Pro17,
            "iPhone-16" | "iPhone-16-Plus" => Body::Iphone16,
            _ => Body::Generic,
        }
    }

    /// Its model's file name beside the helper (see `helper`).
    fn model(self) -> &'static str {
        match self {
            Body::Duo => "iphone-duo-open.usdz",
            Body::Air => "iphone-air.usdz",
            Body::Pro17 => "iphone-17-pro.usdz",
            Body::Iphone16 => "iphone-16.usdz",
            Body::Generic => "iphone.usdz",
        }
    }
}

impl Device {
    /// A device as `leaf sim list` would list it, for other modules' tests.
    #[cfg(test)]
    pub fn listed(udid: &str, name: &str, runtime: &str, state: &str) -> Device {
        Device {
            udid: udid.into(),
            name: name.into(),
            runtime: runtime.into(),
            state: state.into(),
            body: Body::for_device_type(""),
            version: Vec::new(),
        }
    }

    pub fn booted(&self) -> bool {
        self.state == "Booted"
    }
}

#[derive(Deserialize)]
struct Listing {
    devices: BTreeMap<String, Vec<Listed>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    udid: String,
    name: String,
    state: String,
    #[serde(default = "available")]
    is_available: bool,
    #[serde(default)]
    device_type_identifier: String,
}

fn available() -> bool {
    true
}

/// A runtime identifier such as `com.apple.CoreSimulator.SimRuntime.iOS-26-1`
/// as its platform and version. A device-specific runtime adds the model
/// after the version (`iOS-27-2-iPhone19-4`); the version stops there.
fn runtime(identifier: &str) -> Option<(&str, Vec<u32>)> {
    let tail = identifier.rsplit('.').next()?;
    let mut parts = tail.split('-');
    let platform = parts.next()?;
    let version: Vec<u32> = parts.map_while(|part| part.parse().ok()).collect();
    (!version.is_empty()).then_some((platform, version))
}

/// The iPhones in `simctl list devices available -j` output: booted first,
/// then the newest iOS, then by name.
fn parse_devices(json: &str) -> Result<Vec<Device>, String> {
    let listing: Listing = serde_json::from_str(json)
        .map_err(|e| format!("simctl's device list couldn't be read ({e})"))?;
    let mut devices: Vec<Device> = listing
        .devices
        .iter()
        .filter_map(|(identifier, listed)| {
            let (platform, version) = runtime(identifier)?;
            (platform == "iOS").then_some((version, listed))
        })
        .flat_map(|(version, listed)| {
            listed
                .iter()
                .filter(|device| device.is_available)
                .filter(|device| {
                    device.device_type_identifier.contains("iPhone")
                        || (device.device_type_identifier.is_empty()
                            && device.name.starts_with("iPhone"))
                })
                .map(move |device| Device {
                    udid: device.udid.clone(),
                    name: device.name.clone(),
                    runtime: format!(
                        "iOS {}",
                        version
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(".")
                    ),
                    state: device.state.clone(),
                    body: Body::for_device_type(&device.device_type_identifier),
                    version: version.clone(),
                })
        })
        .collect();
    devices.sort_by(|a, b| {
        b.booted()
            .cmp(&a.booted())
            .then_with(|| b.version.cmp(&a.version))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(devices)
}

/// What went wrong running a `leaf sim` command, for the person or agent.
#[derive(Debug, PartialEq, Eq)]
enum SimError {
    /// Plain message, already saying what to do.
    Message(String),
    /// Bad arguments: print the usage.
    Usage,
}

impl From<String> for SimError {
    fn from(message: String) -> Self {
        SimError::Message(message)
    }
}

/// The Xcode developer folder simulators come from.
fn developer_dir() -> Result<PathBuf, String> {
    let dir = match std::env::var_os("DEVELOPER_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let output = Command::new("/usr/bin/xcode-select")
                .arg("-p")
                .output()
                .map_err(|e| format!("xcode-select couldn't run ({e}). Install Xcode from the App Store to use simulators."))?;
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        }
    };
    if !dir.join("Platforms/iPhoneSimulator.platform").is_dir()
        && !dir
            .join("../SharedFrameworks/SimulatorKit.framework")
            .is_dir()
    {
        return Err(format!(
            "{} isn't a full Xcode, and simulators come with Xcode. Install Xcode, then point at it with `sudo xcode-select -s /Applications/Xcode.app`.",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Runs `xcrun simctl …`, returning its stdout or a message with its stderr.
fn simctl(args: &[&str]) -> Result<String, String> {
    let output = Command::new("/usr/bin/xcrun")
        .arg("simctl")
        .args(args)
        .output()
        .map_err(|e| format!("xcrun couldn't run ({e}). Install Xcode to use simulators."))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let why = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no reason given");
    Err(format!(
        "`xcrun simctl {}` failed: {why}",
        args.first().copied().unwrap_or_default()
    ))
}

pub fn devices() -> Result<Vec<Device>, String> {
    parse_devices(&simctl(&["list", "devices", "available", "-j"])?)
}

const NONE_INSTALLED: &str = "No iPhone simulators are installed. Add an iOS runtime in Xcode → Settings → Components (or run `xcodebuild -downloadPlatform iOS`), then try again.";

/// Where a device's pane listens for `leaf sim` commands.
fn socket_dir() -> Result<PathBuf, String> {
    let home =
        std::env::var_os("HOME").ok_or("HOME is unset, so leaf can't find the simulator panes")?;
    Ok(PathBuf::from(home).join("Library/Caches/leaf/sim"))
}

fn socket_path(udid: &str) -> Result<PathBuf, String> {
    Ok(socket_dir()?.join(format!("{}.sock", udid.to_uppercase())))
}

/// The devices open in a Leaf pane right now, by udid.
fn open_panes() -> Vec<String> {
    let Ok(dir) = socket_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut udids: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let udid = path
                .file_name()?
                .to_str()?
                .strip_suffix(".sock")?
                .to_owned();
            UnixStream::connect(&path).ok().map(|_| udid)
        })
        .collect();
    udids.sort();
    udids
}

/// `--device <udid>` taken out of `args`.
fn take_device(args: &[String]) -> Result<(Option<String>, Vec<String>), SimError> {
    let mut device = None;
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--device" {
            device = Some(iter.next().ok_or(SimError::Usage)?.clone());
        } else if let Some(udid) = arg.strip_prefix("--device=") {
            device = Some(udid.to_owned());
        } else {
            rest.push(arg.clone());
        }
    }
    Ok((device, rest))
}

/// Which device a command means: the one named, else the only one open in a
/// pane, else the only booted one.
fn pick_device(named: Option<String>) -> Result<String, String> {
    if let Some(udid) = named.or_else(|| {
        std::env::var("LEAF_SIM_DEVICE")
            .ok()
            .filter(|udid| !udid.is_empty())
    }) {
        return Ok(udid);
    }
    let panes = open_panes();
    match panes.as_slice() {
        [one] => return Ok(one.clone()),
        [] => {}
        many => {
            return Err(format!(
                "{} simulators are open in Leaf panes ({}). Pick one with --device <udid>.",
                many.len(),
                many.join(", ")
            ));
        }
    }
    let booted: Vec<Device> = devices()?.into_iter().filter(Device::booted).collect();
    match booted.as_slice() {
        [one] => Ok(one.udid.clone()),
        [] => Err("No simulator is booted. Open one with Leaf: Open Simulator… in Rex, or boot one with `xcrun simctl boot <udid>`.".into()),
        many => Err(format!(
            "{} simulators are booted ({}). Pick one with --device <udid>.",
            many.len(),
            many.iter()
                .map(|d| format!("{} {}", d.name, d.udid))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A pane's answer to one request.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Answer {
    Ok(serde_json::Value),
    Failed { message: String },
}

/// Sends one request to the pane showing `udid` and returns its answer.
fn ask_pane(udid: &str, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let path = socket_path(udid)?;
    let mut stream = UnixStream::connect(&path).map_err(|_| {
        format!("Simulator {udid} isn't open in a Leaf pane, so leaf can't touch or type on it. Open it with Leaf: Open Simulator… in Rex, or `leaf sim open {udid}` in a Rex pane.")
    })?;
    // Typing a long text takes a while; nothing else comes close.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(180)));
    let mut line = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|e| format!("The simulator pane stopped listening ({e}). Nothing was sent."))?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).map_err(|e| {
        format!("The simulator pane didn't answer ({e}). The action may or may not have happened; take a screenshot to check.")
    })?;
    match serde_json::from_str::<Answer>(reply.trim()) {
        Ok(Answer::Ok(value)) => Ok(value),
        Ok(Answer::Failed { message }) => Err(message),
        Err(e) => Err(format!(
            "The simulator pane answered with something leaf couldn't read ({e}). Update leaf so the CLI and the pane match."
        )),
    }
}

/// How long after an action a snapshot waits for the screen to react.
const ACTION_GRACE: Duration = Duration::from_millis(1500);

fn action_path(udid: &str) -> Result<PathBuf, String> {
    Ok(socket_dir()?.join(format!("{}.acted", udid.to_uppercase())))
}

/// Notes that something was just done to `udid`; best effort.
fn acted(udid: &str) {
    if let Ok(path) = action_path(udid) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, b"");
    }
}

/// How long ago something was last done to `udid` through leaf.
fn since_action(udid: &str) -> Option<Duration> {
    let modified = std::fs::metadata(action_path(udid).ok()?)
        .ok()?
        .modified()
        .ok()?;
    modified.elapsed().ok()
}

/// Sends `request` to the pane showing `udid` or, with none open, to the
/// helper run headless for this one request.
fn drive(udid: &str, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    let acts = ["tap", "swipe", "type", "key", "button"]
        .iter()
        .any(|action| request.get(action).is_some());
    let answer = drive_once(udid, request);
    if acts {
        acted(udid);
    }
    answer
}

fn drive_once(udid: &str, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    if UnixStream::connect(socket_path(udid)?).is_ok() {
        return ask_pane(udid, request);
    }
    let executable = helper()?;
    let mut child = Command::new(&executable)
        .arg("--drive")
        .env("LEAF_SIM_UDID", udid)
        .env("LEAF_DEVELOPER_DIR", developer_dir()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "couldn't start leaf's simulator driver at {} ({e}). Nothing was sent.",
                executable.display()
            )
        })?;
    if let Some(mut input) = child.stdin.take() {
        let line = serde_json::to_vec(request).map_err(|e| e.to_string())?;
        input.write_all(&line).map_err(|e| {
            format!("leaf's simulator driver stopped early ({e}). Nothing was sent.")
        })?;
    }
    let output = child.wait_with_output().map_err(|e| {
        format!("lost track of leaf's simulator driver ({e}). The action may or may not have happened; take a snapshot to check.")
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    match serde_json::from_str::<Answer>(stdout.trim()) {
        Ok(Answer::Ok(value)) => Ok(value),
        Ok(Answer::Failed { message }) => Err(message),
        Err(_) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let why = stderr.lines().rev().map(str::trim).find(|l| !l.is_empty());
            Err(format!(
                "leaf's simulator driver stopped without answering ({}; {}). The action may or may not have happened; take a snapshot to check.",
                output.status,
                why.unwrap_or("it printed nothing")
            ))
        }
    }
}

/// The frontmost app's elements, once the screen has been still for a
/// moment or after `settle` seconds.
fn tree(udid: &str, settle: f64) -> Result<Tree, String> {
    let value = drive(
        udid,
        &serde_json::json!({ "snapshot": { "settle": settle } }),
    )?;
    serde_json::from_value(value).map_err(|e| {
        format!("The simulator sent its screen in a form leaf couldn't read ({e}). Update leaf so the CLI and the pane match.")
    })
}

/// Where the last snapshot of `udid` is kept, for its refs and `--diff`.
fn snapshot_path(udid: &str) -> Result<PathBuf, String> {
    Ok(socket_dir()?.join(format!("{}.snapshot.json", udid.to_uppercase())))
}

/// The last snapshot of `udid`; an empty one when there's none yet.
fn last_snapshot(udid: &str) -> Snapshot {
    snapshot_path(udid)
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_snapshot(udid: &str, snapshot: &Snapshot) -> Result<(), String> {
    let path = snapshot_path(udid)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    }
    let json = serde_json::to_vec(snapshot).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| {
        format!(
            "couldn't save the snapshot's refs to {} ({e}); refs from it won't work.",
            path.display()
        )
    })
}

/// Where the element `@e<number>` from the last snapshot is now.
fn locate(udid: &str, number: u32) -> Result<(Element, i64, i64), String> {
    let snapshot = last_snapshot(udid);
    let entry = snapshot.entry(number).ok_or_else(|| {
        format!("@e{number} isn't in this simulator's last snapshot. Run `leaf sim snapshot` and use a ref it shows.")
    })?;
    let tree = tree(udid, 1.0)?;
    let element = tree.find(&entry.key).cloned().ok_or_else(|| {
        let name = match (entry.key.label.as_str(), entry.key.id.as_str()) {
            ("", "") => String::new(),
            ("", id) => format!(" #{id}"),
            (label, _) => format!(" \"{label}\""),
        };
        format!(
            "@e{number} ({}{name}) isn't on the screen now. Run `leaf sim snapshot` to see what is.",
            entry.key.role.to_lowercase()
        )
    })?;
    Ok((element, tree.width, tree.height))
}

/// Taps the middle of `@e<number>`.
fn tap_ref(udid: &str, number: u32) -> Result<(), String> {
    let (element, width, height) = locate(udid, number)?;
    let (x, y) = element.tap_point(width, height);
    drive(udid, &serde_json::json!({ "tap": { "at": [x, y] } }))?;
    Ok(())
}

/// A swipe that scrolls `direction` across `area` (x, y, width, height):
/// "down" moves the content up to show what's below.
fn scroll_swipe(direction: &str, area: [i64; 4]) -> Option<([i64; 2], [i64; 2])> {
    let [x, y, w, h] = area;
    let (cx, cy) = (x + w / 2, y + h / 2);
    let (near_x, far_x) = (x + w * 3 / 10, x + w * 7 / 10);
    let (near_y, far_y) = (y + h * 3 / 10, y + h * 7 / 10);
    match direction {
        "down" => Some(([cx, far_y], [cx, near_y])),
        "up" => Some(([cx, near_y], [cx, far_y])),
        "right" => Some(([far_x, cy], [near_x, cy])),
        "left" => Some(([near_x, cy], [far_x, cy])),
        _ => None,
    }
}

/// An installed app.
#[derive(Debug, PartialEq, Eq)]
struct App {
    name: String,
    bundle: String,
    /// Installed by someone, not part of iOS.
    user: bool,
}

/// `simctl listapps`, as JSON (plutil's conversion of its property list).
fn parse_apps(json: &str) -> Result<Vec<App>, String> {
    let listed: BTreeMap<String, serde_json::Value> = serde_json::from_str(json)
        .map_err(|e| format!("couldn't read simctl's list of apps ({e})."))?;
    let mut apps: Vec<App> = listed
        .into_iter()
        .map(|(bundle, info)| {
            let text = |key: &str| info.get(key).and_then(|v| v.as_str()).map(str::to_owned);
            App {
                name: text("CFBundleDisplayName")
                    .or_else(|| text("CFBundleName"))
                    .unwrap_or_else(|| bundle.clone()),
                user: text("ApplicationType").as_deref() == Some("User"),
                bundle,
            }
        })
        .collect();
    apps.sort_by_key(|app| (!app.user, app.name.to_lowercase()));
    Ok(apps)
}

fn apps(udid: &str) -> Result<Vec<App>, String> {
    let plist = simctl(&["listapps", udid])?;
    let mut plutil = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("plutil couldn't run ({e}), so leaf can't read the app list."))?;
    if let Some(mut input) = plutil.stdin.take() {
        input
            .write_all(plist.as_bytes())
            .map_err(|e| format!("plutil stopped early ({e})."))?;
    }
    let output = plutil
        .wait_with_output()
        .map_err(|e| format!("plutil didn't finish ({e})."))?;
    parse_apps(&String::from_utf8_lossy(&output.stdout))
}

/// The bundle id `name` means: itself when it looks like one, else the
/// app with that name, or the only one whose name starts with it.
fn bundle_for(name: &str, apps: &[App]) -> Result<String, String> {
    if let Some(app) = apps.iter().find(|app| app.bundle == name) {
        return Ok(app.bundle.clone());
    }
    let wanted = name.to_lowercase();
    if let Some(app) = apps.iter().find(|app| app.name.to_lowercase() == wanted) {
        return Ok(app.bundle.clone());
    }
    let starting: Vec<&App> = apps
        .iter()
        .filter(|app| app.name.to_lowercase().starts_with(&wanted))
        .collect();
    match starting.as_slice() {
        [one] => Ok(one.bundle.clone()),
        [] => Err(format!(
            "No installed app is called {name}. `leaf sim apps` lists them."
        )),
        many => Err(format!(
            "{name} could be {}. Use the full name or the bundle id.",
            many.iter()
                .map(|app| format!("{} ({})", app.name, app.bundle))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn number(text: &str) -> Result<f64, SimError> {
    text.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .ok_or(SimError::Usage)
}

/// The width and height a PNG file says it has.
fn png_size(path: &Path) -> Option<(u32, u32)> {
    let mut header = [0u8; 24];
    std::fs::File::open(path)
        .ok()?
        .read_exact(&mut header)
        .ok()?;
    if &header[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let width = u32::from_be_bytes(header[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(header[20..24].try_into().ok()?);
    Some((width, height))
}

/// Saves the screen of `udid` to `path`, or to a new file in leaf's cache.
fn screenshot(udid: &str, path: Option<&str>) -> Result<PathBuf, String> {
    let target = match path {
        Some(path) => PathBuf::from(path),
        None => {
            let dir = socket_dir()?.join("shots");
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
            prune(&dir, 40);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default();
            dir.join(format!("{udid}-{stamp}.png"))
        }
    };
    let shown = target.to_string_lossy().into_owned();
    // The iPhone Duo has two built-in screens, and simctl won't pick one. A
    // Leaf pane showing it knows which faces front: the cover ("primary")
    // folded, the inner screen ("primary-1") open. Without one, iOS is on
    // the cover unless something else unfolded it.
    let display = ask_pane(udid, &serde_json::json!({ "info": {} }))
        .ok()
        .and_then(|info| info.get("display")?.as_str().map(str::to_owned));
    match &display {
        // The pane named the screen that faces front; capturing another
        // would show the agent the wrong one, so its error stands.
        Some(display) => simctl(&[
            "io",
            udid,
            "screenshot",
            "--type=png",
            &format!("--display={display}"),
            &shown,
        ])?,
        None => simctl(&["io", udid, "screenshot", "--type=png", &shown]).or_else(|first| {
            simctl(&[
                "io",
                udid,
                "screenshot",
                "--type=png",
                "--display=primary",
                &shown,
            ])
            .map_err(|_| first)
        })?,
    };
    Ok(target)
}

/// Keeps the newest `keep` files in `dir`; best effort.
fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

// Asking the agent about the screen -----------------------------------------

/// A question about the screen, from the helper.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Ask {
    question: String,
    /// x, y, width, height in screenshot pixels.
    #[serde(default)]
    region: Option<[u32; 4]>,
}

/// A line the helper writes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Request {
    Ask(Ask),
    Agent {},
}

/// A line written back to the helper.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Sent { agent: String },
    Failed { message: String, prompt: String },
    Agent { name: Option<String> },
}

/// What a screenshot for a question came to.
enum Shot {
    Saved {
        path: PathBuf,
        size: Option<(u32, u32)>,
    },
    Missing {
        why: String,
    },
}

/// The prompt for the agent: which device, where its screenshot is, the
/// marked region, how to drive it, then the question.
fn prompt(device: &Device, shot: &Shot, ask: &Ask) -> String {
    let mut out = format!(
        "From the iOS Simulator open in leaf: {} ({}), device {}.\n",
        device.name, device.runtime, device.udid
    );
    match shot {
        Shot::Saved { path, size } => {
            out.push_str(&format!(
                "A screenshot of its screen is at {}",
                path.display()
            ));
            if let Some((width, height)) = size {
                out.push_str(&format!(" ({width}×{height} pixels)"));
            }
            out.push_str(".\n");
            if let Some([x, y, width, height]) = ask.region {
                out.push_str(&format!(
                    "The person marked the part of it at x {x}, y {y}, {width} wide and {height} tall, in the screenshot's pixels from the top left.\n"
                ));
            }
        }
        Shot::Missing { why } => {
            out.push_str(&format!(
                "leaf couldn't save a screenshot ({why}); take one with `leaf sim screenshot --device {}`.\n",
                device.udid
            ));
        }
    }
    out.push_str(&format!(
        "You can drive this simulator with `leaf sim snapshot` (its elements, each with a ref like @e3), `leaf sim tap @e3` or `leaf sim tap X Y`, `leaf sim swipe X1 Y1 X2 Y2`, `leaf sim type TEXT`, `leaf sim key NAME`, `leaf sim button home` and `leaf sim screenshot`, in screenshot pixels, adding `--device {}`.\n\n",
        device.udid
    ));
    let question = match ask.question.trim() {
        "" => "Explain what's on this screen and where it comes from in the code.",
        question => question,
    };
    out.push_str(question);
    out.push('\n');
    out
}

/// Answers one line from the helper.
fn answer(device: &Device, line: &str) -> Reply {
    let ask = match serde_json::from_str::<Request>(line) {
        Ok(Request::Ask(ask)) => ask,
        Ok(Request::Agent {}) => {
            return Reply::Agent {
                name: crate::agent::current(),
            };
        }
        Err(error) => {
            return Reply::Failed {
                message: format!(
                    "leaf couldn't read the simulator's request ({error}). Nothing was sent."
                ),
                prompt: String::new(),
            };
        }
    };
    let shot = match screenshot(&device.udid, None) {
        Ok(path) => Shot::Saved {
            size: png_size(&path),
            path,
        },
        Err(why) => Shot::Missing { why },
    };
    let prompt = prompt(device, &shot, &ask);
    match crate::agent::deliver(&prompt) {
        Ok(agent) => Reply::Sent {
            agent: agent.command,
        },
        Err(error) => Reply::Failed {
            message: error.to_string(),
            prompt,
        },
    }
}

/// The helper on disk, with the Duo's hinge helper beside it. The phone
/// models are downloaded apart, by `leaf sim install` (see simmodels.rs).
fn helper() -> Result<PathBuf, String> {
    if SIM_HOST.is_empty() {
        return Err("this leaf was built without the simulator pane (Apple's Swift toolchain wasn't available). Install Xcode, then reinstall leaf.".into());
    }
    let dir = crate::helper::unpack(
        "sim",
        "simulator",
        &[
            Part {
                name: "leaf-sim-host",
                bytes: SIM_HOST,
                mode: 0o755,
            },
            Part {
                name: "leaf-duo-hinge",
                bytes: DUO_HINGE,
                mode: 0o755,
            },
        ],
    )?;
    Ok(dir.join("leaf-sim-host"))
}

/// `leaf sim open <udid>`: the device in this Rex pane until it closes.
fn open(udid: &str) -> Result<(), String> {
    let Some(block) = crate::agent::rex_block() else {
        return Err("open the simulator from a Rex pane, or with Leaf: Open Simulator… in Rex's command palette.".into());
    };
    // Newer Rex sets only REX_BLOCK in a pane; the helper needs the session too.
    let session = crate::agent::rex_session_api_id()
        .map_err(|error| format!("couldn't find this pane's Rex session: {error}"))?;
    let developer = developer_dir()?;
    let listed = devices()?;
    if listed.is_empty() {
        return Err(NONE_INSTALLED.into());
    }
    let device = listed
        .into_iter()
        .find(|d| d.udid.eq_ignore_ascii_case(udid))
        .ok_or_else(|| {
            format!(
                "No installed iPhone simulator has the id {udid}. `leaf sim list` shows the ids."
            )
        })?;
    let socket = socket_path(&device.udid)?;
    if UnixStream::connect(&socket).is_ok() {
        return Err(format!(
            "{} is already open in another Leaf pane. Use that one, or close it first.",
            device.name
        ));
    }
    let model = crate::simmodels::path(device.body.model())?.ok_or_else(|| {
        "the simulator's 3D iPhones aren't installed. Run `leaf sim install` (it lists them and asks before downloading), then open the simulator again.".to_owned()
    })?;
    let executable = helper()?;
    let rex = crate::agent::rex_program()?;
    let mut command = Command::new(&executable);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("REX_SESSION", session)
        .env("REX_BLOCK", block)
        .env("LEAF_REX", rex)
        .env(
            "LEAF_AGENT",
            crate::agent::default_agent().unwrap_or_default(),
        )
        .env("LEAF_SIM_UDID", &device.udid)
        .env("LEAF_SIM_NAME", &device.name)
        .env("LEAF_SIM_RUNTIME", &device.runtime)
        .env("LEAF_SIM_SOCKET", &socket)
        .env("LEAF_DEVELOPER_DIR", &developer);
    command.env("LEAF_SIM_MODEL", model);
    if device.body == Body::Duo {
        command.env("LEAF_SIM_DUO", "1");
    } else {
        command.env_remove("LEAF_SIM_DUO");
    }
    let mut child = command.spawn().map_err(|e| {
        format!(
            "couldn't start the simulator pane at {} ({e}).",
            executable.display()
        )
    })?;
    if let (Some(mut to_helper), Some(from_helper)) = (child.stdin.take(), child.stdout.take()) {
        for line in BufReader::new(from_helper).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let reply = answer(&device, &line);
            let written = serde_json::to_string(&reply)
                .map_err(std::io::Error::other)
                .and_then(|text| writeln!(to_helper, "{text}"))
                .and_then(|()| to_helper.flush());
            // The helper is gone; its exit status says why below.
            if written.is_err() {
                break;
            }
        }
    }
    match child.wait() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "the simulator pane stopped unexpectedly ({status})."
        )),
        Err(error) => Err(format!("lost track of the simulator pane ({error}).")),
    }
}

fn print_value(value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) if map.is_empty() => {}
        serde_json::Value::Null => {}
        other => println!("{other}"),
    }
}

fn command(args: &[String]) -> Result<(), SimError> {
    let (named, args) = take_device(args)?;
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["list"] | [] => {
            let listed = devices()?;
            if listed.is_empty() {
                return Err(SimError::Message(NONE_INSTALLED.into()));
            }
            for device in listed {
                let state = if device.booted() { "  booted" } else { "" };
                println!(
                    "{}  {} ({}){state}",
                    device.udid, device.name, device.runtime
                );
            }
            Ok(())
        }
        ["list", "--json"] => {
            let json = serde_json::to_string(&devices()?).map_err(|e| e.to_string())?;
            println!("{json}");
            Ok(())
        }
        // Rows for Rex's palette.
        ["--list"] => {
            let rows = crate::palette::simulators(&devices()?);
            let json = serde_json::to_string(&rows).map_err(|e| e.to_string())?;
            println!("{json}");
            Ok(())
        }
        ["help"] | ["--help"] | ["-h"] => {
            println!("{USAGE}");
            Ok(())
        }
        ["install"] => Ok(crate::simmodels::install(false).map_err(String::from)?),
        ["install", "--yes" | "-y"] => Ok(crate::simmodels::install(true).map_err(String::from)?),
        ["open", udid] => Ok(open(udid)?),
        // Unpack the helper and print where, so a release can prove the
        // pane it ships runs, outside Rex.
        ["--check"] => {
            println!("{}", helper()?.display());
            Ok(())
        }
        ["info"] => {
            let udid = pick_device(named)?;
            let mut info = drive(&udid, &serde_json::json!({ "info": {} }))?;
            // Run headless, the helper doesn't know the device's name.
            if info.get("name").and_then(|n| n.as_str()) == Some("")
                && let Some(device) = devices()?.into_iter().find(|d| d.udid == udid)
            {
                info["name"] = device.name.into();
                info["runtime"] = device.runtime.into();
            }
            print_value(&info);
            Ok(())
        }
        ["snapshot", flags @ ..] => {
            let (mut detail, mut diff) = (Detail::Full, false);
            for flag in flags {
                match *flag {
                    "-i" | "--interactive" => detail = Detail::Interactive,
                    "--diff" | "-d" => diff = true,
                    _ => return Err(SimError::Usage),
                }
            }
            let udid = pick_device(named)?;
            let before = last_snapshot(&udid);
            // Right after an action the screen may not have started to
            // change (going home takes about a second), so a still screen
            // that matches the last snapshot isn't trusted until then.
            let mut snapshot = tree(&udid, 2.0)?.snapshot(&before, detail);
            while snapshot.screen == before.screen
                && since_action(&udid).is_some_and(|since| since < ACTION_GRACE)
            {
                std::thread::sleep(Duration::from_millis(150));
                snapshot = tree(&udid, 2.0)?.snapshot(&before, detail);
            }
            save_snapshot(&udid, &snapshot)?;
            if diff && !before.text.is_empty() {
                print!("{}", snapshot.diff(&before));
            } else {
                print!("{}", snapshot.text);
            }
            Ok(())
        }
        ["screenshot"] | ["screenshot", _] => {
            let udid = pick_device(named)?;
            let path = screenshot(&udid, words.get(1).copied())?;
            println!("{}", path.display());
            Ok(())
        }
        ["tap", target] => {
            let number = parse_ref(target).ok_or(SimError::Usage)?;
            let udid = pick_device(named)?;
            Ok(tap_ref(&udid, number)?)
        }
        ["tap", x, y] => {
            let udid = pick_device(named)?;
            let at = [number(x)?, number(y)?];
            drive(&udid, &serde_json::json!({ "tap": { "at": at } }))?;
            Ok(())
        }
        ["swipe", x1, y1, x2, y2, rest @ ..] if rest.len() <= 1 => {
            let udid = pick_device(named)?;
            let duration = rest.first().map(|s| number(s)).transpose()?.unwrap_or(0.3);
            let request = serde_json::json!({ "swipe": {
                "from": [number(x1)?, number(y1)?],
                "to": [number(x2)?, number(y2)?],
                "duration": duration,
            }});
            drive(&udid, &request)?;
            Ok(())
        }
        ["scroll", direction, rest @ ..] if rest.len() <= 1 => {
            let udid = pick_device(named)?;
            let area = match rest.first() {
                Some(target) => {
                    let number = parse_ref(target).ok_or(SimError::Usage)?;
                    locate(&udid, number)?.0.frame
                }
                None => {
                    let info = drive(&udid, &serde_json::json!({ "info": {} }))?;
                    let size = |key: &str| info.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
                    [0, 0, size("width"), size("height")]
                }
            };
            let (from, to) = scroll_swipe(direction, area).ok_or(SimError::Usage)?;
            let request =
                serde_json::json!({ "swipe": { "from": from, "to": to, "duration": 0.4 } });
            drive(&udid, &request)?;
            Ok(())
        }
        ["type", target, text @ ..] if !text.is_empty() && parse_ref(target).is_some() => {
            let number = parse_ref(target).ok_or(SimError::Usage)?;
            let udid = pick_device(named)?;
            tap_ref(&udid, number)?;
            // The keyboard takes a moment to come up.
            std::thread::sleep(Duration::from_millis(300));
            drive(
                &udid,
                &serde_json::json!({ "type": { "text": text.join(" ") } }),
            )?;
            Ok(())
        }
        ["type", text @ ..] if !text.is_empty() => {
            let udid = pick_device(named)?;
            drive(
                &udid,
                &serde_json::json!({ "type": { "text": text.join(" ") } }),
            )?;
            Ok(())
        }
        ["fill", target, text @ ..] => {
            let number = parse_ref(target).ok_or(SimError::Usage)?;
            let udid = pick_device(named)?;
            tap_ref(&udid, number)?;
            std::thread::sleep(Duration::from_millis(300));
            for key in ["cmd+a", "backspace"] {
                drive(&udid, &serde_json::json!({ "key": { "name": key } }))?;
            }
            if !text.is_empty() {
                drive(
                    &udid,
                    &serde_json::json!({ "type": { "text": text.join(" ") } }),
                )?;
            }
            Ok(())
        }
        ["key", name] => {
            let udid = pick_device(named)?;
            drive(&udid, &serde_json::json!({ "key": { "name": name } }))?;
            Ok(())
        }
        ["button", name] => {
            let udid = pick_device(named)?;
            drive(&udid, &serde_json::json!({ "button": { "name": name } }))?;
            Ok(())
        }
        ["wait", text, rest @ ..] if rest.len() <= 1 => {
            let seconds = rest.first().map(|s| number(s)).transpose()?.unwrap_or(10.0);
            let udid = pick_device(named)?;
            let wanted = text.to_lowercase();
            // A timeout too long for a Duration is a typo, not a wish.
            let timeout =
                Duration::try_from_secs_f64(seconds.max(0.0)).map_err(|_| SimError::Usage)?;
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let before = last_snapshot(&udid);
                let snapshot = tree(&udid, 0.0)?.snapshot(&before, Detail::Full);
                let found: Vec<&str> = snapshot
                    .text
                    .lines()
                    .skip(1)
                    .filter(|line| line.to_lowercase().contains(&wanted))
                    .collect();
                if !found.is_empty() {
                    for line in found {
                        println!("{}", line.trim_start());
                    }
                    save_snapshot(&udid, &snapshot)?;
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(SimError::Message(format!(
                        "Nothing on the screen showed \"{text}\" within {seconds} s. Run `leaf sim snapshot` to see what's there."
                    )));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        ["apps"] => {
            let udid = pick_device(named)?;
            for app in apps(&udid)? {
                let mark = if app.user { "" } else { "  (iOS)" };
                println!("{}  {}{mark}", app.name, app.bundle);
            }
            Ok(())
        }
        ["open-url", url] => {
            let udid = pick_device(named)?;
            simctl(&["openurl", &udid, url])?;
            acted(&udid);
            Ok(())
        }
        ["launch", name @ ..] if !name.is_empty() => {
            let udid = pick_device(named)?;
            let installed = apps(&udid)?;
            let bundle = bundle_for(&name.join(" "), &installed)?;
            simctl(&["launch", &udid, &bundle])?;
            acted(&udid);
            // simctl returns once the app runs; wait until it's in front, so
            // the next snapshot shows it. Its accessibility name is usually
            // its display name.
            let Some(app) = installed.iter().find(|app| app.bundle == bundle) else {
                return Ok(());
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                let front = tree(&udid, 0.0)?;
                if front
                    .app_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&app.name))
                {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(150));
            }
            // simctl sometimes starts an app it just terminated in the background.
            eprintln!(
                "leaf: {} is running but didn't come to the front within 3 s. Run `leaf sim snapshot` to check; if it's not there, tap its icon on the home screen.",
                app.name
            );
            Ok(())
        }
        _ => Err(SimError::Usage),
    }
}

pub fn run(args: &[String]) -> ExitCode {
    if !cfg!(target_os = "macos") {
        eprintln!("leaf: iOS simulators run on macOS only.");
        return ExitCode::FAILURE;
    }
    if let Some(version) = macos_version()
        && version < (13, 0)
    {
        eprintln!(
            "leaf: the simulator pane needs macOS 13 or later; this Mac runs {}.{}. Everything else in leaf works here.",
            version.0, version.1
        );
        return ExitCode::FAILURE;
    }
    match command(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(SimError::Usage) => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(SimError::Message(message)) => {
            eprintln!("leaf: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"{"devices": {
        "com.apple.CoreSimulator.SimRuntime.iOS-18-6": [
            {"udid": "A1", "name": "iPhone 16", "state": "Shutdown", "isAvailable": true,
             "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-16"}
        ],
        "com.apple.CoreSimulator.SimRuntime.iOS-26-1": [
            {"udid": "B2", "name": "iPhone 17 Pro", "state": "Shutdown", "isAvailable": true,
             "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"},
            {"udid": "C3", "name": "iPad Air 11-inch (M3)", "state": "Shutdown", "isAvailable": true,
             "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPad-Air-11-inch-M3"},
            {"udid": "D4", "name": "iPhone Air", "state": "Booted", "isAvailable": true,
             "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-Air"},
            {"udid": "E5", "name": "iPhone 17", "state": "Shutdown", "isAvailable": false,
             "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17"}
        ],
        "com.apple.CoreSimulator.SimRuntime.watchOS-26-0": [
            {"udid": "F6", "name": "Apple Watch Ultra 3", "state": "Shutdown", "isAvailable": true,
             "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.Apple-Watch-Ultra-3"}
        ]
    }}"#;

    #[test]
    fn lists_available_iphones_booted_then_newest() {
        let devices = parse_devices(LISTING).expect("parses");
        let names: Vec<(&str, &str)> = devices
            .iter()
            .map(|d| (d.name.as_str(), d.runtime.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("iPhone Air", "iOS 26.1"),
                ("iPhone 17 Pro", "iOS 26.1"),
                ("iPhone 16", "iOS 18.6"),
            ]
        );
        assert!(devices[0].booted());
    }

    #[test]
    fn an_empty_listing_has_no_devices() {
        assert_eq!(parse_devices(r#"{"devices": {}}"#), Ok(Vec::new()));
        assert!(parse_devices("not json").is_err());
    }

    #[test]
    fn runtimes_parse() {
        assert_eq!(
            runtime("com.apple.CoreSimulator.SimRuntime.iOS-26-1"),
            Some(("iOS", vec![26, 1]))
        );
        assert_eq!(
            runtime("com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4"),
            Some(("iOS", vec![27, 2]))
        );
        assert_eq!(runtime("com.apple.CoreSimulator.SimRuntime.odd-x"), None);
    }

    #[test]
    fn the_iphone_duo_on_its_own_runtime_is_listed_once() {
        let listing = r#"{"devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-27-2": [
                {"udid": "A1", "name": "iPhone 18 Pro", "state": "Shutdown", "isAvailable": true,
                 "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-18-Pro"}
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-27-2-iPhone19-4": [
                {"udid": "B2", "name": "iPhone Duo", "state": "Shutdown", "isAvailable": true,
                 "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-Duo"}
            ]
        }}"#;
        let devices = parse_devices(listing).expect("parses");
        let rows: Vec<(&str, &str, bool)> = devices
            .iter()
            .map(|d| (d.name.as_str(), d.runtime.as_str(), d.body == Body::Duo))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("iPhone 18 Pro", "iOS 27.2", false),
                ("iPhone Duo", "iOS 27.2", true),
            ]
        );
        let ids: Vec<String> = crate::palette::simulators(&devices)
            .into_iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(ids, ["A1", "B2"]);
    }

    #[test]
    fn each_device_type_gets_its_body() {
        let body = |name: &str| {
            Body::for_device_type(&format!("com.apple.CoreSimulator.SimDeviceType.{name}"))
        };
        assert_eq!(body("iPhone-Duo"), Body::Duo);
        assert_eq!(body("iPhone-Air"), Body::Air);
        assert_eq!(body("iPhone-17-Pro"), Body::Pro17);
        assert_eq!(body("iPhone-17-Pro-Max"), Body::Pro17);
        assert_eq!(body("iPhone-16"), Body::Iphone16);
        assert_eq!(body("iPhone-16-Plus"), Body::Iphone16);
        assert_eq!(body("iPhone-16-Pro"), Body::Generic);
        assert_eq!(body("iPhone-17"), Body::Generic);
        assert_eq!(Body::for_device_type(""), Body::Generic);
    }

    #[test]
    fn device_flag_comes_out_anywhere() {
        let args: Vec<String> = ["tap", "--device", "B2", "10", "20"]
            .map(String::from)
            .to_vec();
        let (device, rest) = take_device(&args).expect("parses");
        assert_eq!(device.as_deref(), Some("B2"));
        assert_eq!(rest, vec!["tap", "10", "20"]);
        let (device, _) = take_device(&["--device=C3".to_owned()]).expect("parses");
        assert_eq!(device.as_deref(), Some("C3"));
        assert_eq!(take_device(&["--device".to_owned()]), Err(SimError::Usage));
    }

    #[test]
    fn numbers_must_be_finite() {
        assert_eq!(number("12.5"), Ok(12.5));
        assert_eq!(number("NaN"), Err(SimError::Usage));
        assert_eq!(number("x"), Err(SimError::Usage));
    }

    fn device() -> Device {
        Device {
            udid: "B2".into(),
            name: "iPhone 17 Pro".into(),
            runtime: "iOS 26.1".into(),
            state: "Booted".into(),
            body: Body::Pro17,
            version: vec![26, 1],
        }
    }

    #[test]
    fn apps_list_installed_ones_first_by_name() {
        let json = r#"{
            "com.apple.Preferences": {"CFBundleDisplayName": "Settings", "ApplicationType": "System"},
            "com.example.Notes": {"CFBundleName": "Notes Pro", "ApplicationType": "User"},
            "com.apple.mobilesafari": {"CFBundleDisplayName": "Safari", "ApplicationType": "System"},
            "com.apple.bare": {}
        }"#;
        let apps = parse_apps(json).expect("parses");
        let names: Vec<&str> = apps.iter().map(|app| app.name.as_str()).collect();
        assert_eq!(names, ["Notes Pro", "com.apple.bare", "Safari", "Settings"]);
        assert!(apps[0].user);
    }

    #[test]
    fn apps_are_found_by_bundle_name_or_unique_start() {
        let app = |name: &str, bundle: &str| App {
            name: name.into(),
            bundle: bundle.into(),
            user: false,
        };
        let apps = [
            app("Settings", "com.apple.Preferences"),
            app("Safari", "com.apple.mobilesafari"),
            app("Shortcuts", "com.apple.shortcuts"),
        ];
        assert_eq!(
            bundle_for("settings", &apps).as_deref(),
            Ok("com.apple.Preferences")
        );
        assert_eq!(
            bundle_for("Saf", &apps).as_deref(),
            Ok("com.apple.mobilesafari")
        );
        assert_eq!(
            bundle_for("com.apple.shortcuts", &apps).as_deref(),
            Ok("com.apple.shortcuts")
        );
        let many = bundle_for("S", &apps).expect_err("ambiguous");
        assert!(many.contains("Settings (com.apple.Preferences)"));
        assert!(bundle_for("Maps", &apps).is_err());
    }

    #[test]
    fn scrolling_down_swipes_up_inside_the_area() {
        let area = [0, 100, 1000, 1000];
        assert_eq!(scroll_swipe("down", area), Some(([500, 800], [500, 400])));
        assert_eq!(scroll_swipe("up", area), Some(([500, 400], [500, 800])));
        assert_eq!(scroll_swipe("right", area), Some(([700, 600], [300, 600])));
        assert_eq!(scroll_swipe("sideways", area), None);
    }

    #[test]
    fn prompt_names_the_shot_region_and_controls() {
        let ask = Ask {
            question: "Why is this button grey?".into(),
            region: Some([10, 20, 300, 80]),
        };
        let shot = Shot::Saved {
            path: PathBuf::from("/tmp/shot.png"),
            size: Some((1206, 2622)),
        };
        let prompt = prompt(&device(), &shot, &ask);
        assert!(prompt.starts_with(
            "From the iOS Simulator open in leaf: iPhone 17 Pro (iOS 26.1), device B2."
        ));
        assert!(prompt.contains("/tmp/shot.png (1206×2622 pixels)"));
        assert!(prompt.contains("x 10, y 20, 300 wide and 80 tall"));
        assert!(prompt.contains("`leaf sim tap X Y`"));
        assert!(prompt.contains("--device B2"));
        assert!(prompt.ends_with("\n\nWhy is this button grey?\n"));
    }

    #[test]
    fn prompt_without_a_shot_or_question_still_asks() {
        let ask = Ask {
            question: "  ".into(),
            region: Some([1, 2, 3, 4]),
        };
        let shot = Shot::Missing {
            why: "simctl failed".into(),
        };
        let prompt = prompt(&device(), &shot, &ask);
        assert!(prompt.contains("couldn't save a screenshot (simctl failed)"));
        assert!(!prompt.contains("marked"), "no region without a picture");
        assert!(
            prompt
                .ends_with("Explain what's on this screen and where it comes from in the code.\n")
        );
    }

    #[test]
    fn requests_and_replies_match_the_helper() {
        let Ok(Request::Ask(ask)) =
            serde_json::from_str::<Request>(r#"{"ask":{"question":"q","region":[1,2,3,4]}}"#)
        else {
            panic!("an ask parses");
        };
        assert_eq!(ask.region, Some([1, 2, 3, 4]));
        let Ok(Request::Ask(whole)) =
            serde_json::from_str::<Request>(r#"{"ask":{"question":"q"}}"#)
        else {
            panic!("an ask without a region parses");
        };
        assert_eq!(whole.region, None);
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"agent":{}}"#),
            Ok(Request::Agent {})
        ));
        let sent = serde_json::to_string(&Reply::Sent {
            agent: "claude".into(),
        })
        .expect("serializes");
        assert_eq!(sent, r#"{"sent":{"agent":"claude"}}"#);
    }

    #[test]
    fn pane_answers_parse() {
        assert_eq!(
            serde_json::from_str::<Answer>(r#"{"ok":{}}"#).ok(),
            Some(Answer::Ok(serde_json::json!({})))
        );
        assert_eq!(
            serde_json::from_str::<Answer>(r#"{"failed":{"message":"no"}}"#).ok(),
            Some(Answer::Failed {
                message: "no".into()
            })
        );
    }

    #[test]
    fn png_size_reads_the_header() {
        let dir = std::env::temp_dir().join(format!("leaf-sim-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("a.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&1206u32.to_be_bytes());
        bytes.extend_from_slice(&2622u32.to_be_bytes());
        std::fs::write(&path, &bytes).expect("write");
        assert_eq!(png_size(&path), Some((1206, 2622)));
        std::fs::write(&path, b"not a png at all, no sir").expect("write");
        assert_eq!(png_size(&path), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
