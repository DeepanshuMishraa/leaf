//! `leaf sim`: an installed iOS Simulator on a 3D iPhone in a Rex pane, on
//! macOS, and the commands an agent drives it with.
//!
//! The pane is a native helper (src/simulator.swift and src/pane.swift,
//! compiled by build.rs and embedded here; see src/helper.rs), unpacked with
//! the iPhone and iPhone Duo models beside it. It boots the device, shows its
//! live screen and sends touches and keys. It also listens on a Unix socket, one per
//! device, which `leaf sim tap|swipe|type|key|button|info` talk to.
//! `screenshot`, `open-url` and `launch` go through `simctl` and work on any
//! booted simulator.
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

const SIM_HOST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/leaf-sim-host"));
const PHONE: &[u8] = include_bytes!("../vendor/devices/iphone.usdz");
const DUO_OPEN: &[u8] = include_bytes!("../vendor/devices/iphone-duo-open.usdz");

const USAGE: &str = "Usage: leaf sim list [--json]          installed iPhone simulators
       leaf sim open <udid>              show one in this Rex pane, on a 3D iPhone
       leaf sim info                     the device and its screen size in pixels
       leaf sim screenshot [file.png]    save the screen; prints the file's path
       leaf sim tap <x> <y>              tap, in screenshot pixels (0,0 is the top left)
       leaf sim swipe <x1> <y1> <x2> <y2> [seconds]
       leaf sim type <text>              type on the simulator's keyboard
       leaf sim key <name>               enter, escape, backspace, tab, space, up, down, left, right…
       leaf sim button <name>            home, lock, side or siri
       leaf sim open-url <url>           open a link or deep link
       leaf sim launch <bundle id>       start an installed app
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
    /// The foldable iPhone Duo, drawn on its own model.
    #[serde(skip)]
    duo: bool,
    #[serde(skip)]
    version: Vec<u32>,
}

impl Device {
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
                    duo: device.device_type_identifier.ends_with(".iPhone-Duo"),
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
/// pane, else (when `booted_will_do`) the only booted one.
fn pick_device(named: Option<String>, booted_will_do: bool) -> Result<String, String> {
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
    if !booted_will_do {
        return Err("No simulator is open in a Leaf pane. Open one with Leaf: Open Simulator… in Rex's command palette, or `leaf sim open <udid>` in a Rex pane (`leaf sim list` shows the ids).".into());
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
    // The iPhone Duo has two built-in screens, and simctl won't pick one;
    // iOS draws on the cover, the "primary" screen.
    simctl(&["io", udid, "screenshot", "--type=png", &shown]).or_else(|first| {
        simctl(&[
            "io",
            udid,
            "screenshot",
            "--type=png",
            "--display=primary",
            &shown,
        ])
        .map_err(|_| first)
    })?;
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
        "You can drive this simulator with `leaf sim tap X Y`, `leaf sim swipe X1 Y1 X2 Y2`, `leaf sim type TEXT`, `leaf sim key NAME`, `leaf sim button home` and `leaf sim screenshot`, in screenshot pixels, adding `--device {}`.\n\n",
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

/// The helper on disk, with the phone models beside it.
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
                name: "iphone.usdz",
                bytes: PHONE,
                mode: 0o644,
            },
            Part {
                name: "iphone-duo-open.usdz",
                bytes: DUO_OPEN,
                mode: 0o644,
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
    if device.duo {
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
        ["open", udid] => Ok(open(udid)?),
        // Unpack the helper and print where, so a release can prove the
        // pane it ships runs, outside Rex.
        ["--check"] => {
            println!("{}", helper()?.display());
            Ok(())
        }
        ["info"] => {
            let udid = pick_device(named, false)?;
            print_value(&ask_pane(&udid, &serde_json::json!({ "info": {} }))?);
            Ok(())
        }
        ["screenshot"] | ["screenshot", _] => {
            let udid = pick_device(named, true)?;
            let path = screenshot(&udid, words.get(1).copied())?;
            println!("{}", path.display());
            Ok(())
        }
        ["tap", x, y] => {
            let udid = pick_device(named, false)?;
            let at = [number(x)?, number(y)?];
            ask_pane(&udid, &serde_json::json!({ "tap": { "at": at } }))?;
            Ok(())
        }
        ["swipe", x1, y1, x2, y2, rest @ ..] if rest.len() <= 1 => {
            let udid = pick_device(named, false)?;
            let duration = rest.first().map(|s| number(s)).transpose()?.unwrap_or(0.3);
            let request = serde_json::json!({ "swipe": {
                "from": [number(x1)?, number(y1)?],
                "to": [number(x2)?, number(y2)?],
                "duration": duration,
            }});
            ask_pane(&udid, &request)?;
            Ok(())
        }
        ["type", text @ ..] if !text.is_empty() => {
            let udid = pick_device(named, false)?;
            ask_pane(
                &udid,
                &serde_json::json!({ "type": { "text": text.join(" ") } }),
            )?;
            Ok(())
        }
        ["key", name] => {
            let udid = pick_device(named, false)?;
            ask_pane(&udid, &serde_json::json!({ "key": { "name": name } }))?;
            Ok(())
        }
        ["button", name] => {
            let udid = pick_device(named, false)?;
            ask_pane(&udid, &serde_json::json!({ "button": { "name": name } }))?;
            Ok(())
        }
        ["open-url", url] => {
            let udid = pick_device(named, true)?;
            simctl(&["openurl", &udid, url])?;
            Ok(())
        }
        ["launch", bundle] => {
            let udid = pick_device(named, true)?;
            simctl(&["launch", &udid, bundle])?;
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
            .map(|d| (d.name.as_str(), d.runtime.as_str(), d.duo))
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
            duo: false,
            version: vec![26, 1],
        }
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
