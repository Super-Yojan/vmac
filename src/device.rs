//! A phone's screen in a window, and taps, swipes and keys sent back to it:
//! an Android device through adb, an iOS simulator through idb.
//!
//! A device is named by an Android serial (`emulator-5554`, a phone's), an
//! emulator's name (`avd:Pixel_9`) before it has one (the serial is found
//! once it's running), or a simulator's UDID (`ios:UDID`).
//!
//! The screen comes as H.264, decoded here: from `screenrecord
//! --output-format=h264` on Android, `idb video-stream` on iOS. Input goes
//! through a shell kept open per device, so each command doesn't wait for
//! one to start: `adb shell` running `input tap`, `input swipe`, `input
//! keyevent` and `input text`, or a local one running `idb ui ...`.
//!
//! An emulator takes input through its own control channels instead, where
//! `input` takes a quarter of a second to start: touches and text through
//! its console (a TCP socket on its port, 5554 for emulator-5554), in a
//! couple of milliseconds, the touch going down, moving and coming up as
//! the mouse does; keys and buttons through its gRPC service (the console's
//! `event send` is taken and dropped), a unary call sent with curl. A
//! simulator takes them through idb_companion's hid call (on its socket),
//! kept open in a curl streaming each event as it happens, where the idb
//! command takes a quarter of a second to start.

use std::collections::HashMap;
use std::io::Write as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// adb: the Android SDK's (ANDROID_HOME, ANDROID_SDK_ROOT, else
/// ~/Library/Android/sdk), else the one on PATH.
pub fn adb() -> String {
    let sdk = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Android/sdk")));
    sdk.map(|s| s.join("platform-tools/adb"))
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "adb".into())
}

fn adb_output(args: &[&str]) -> Option<String> {
    let out = Command::new(adb()).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The serials adb lists as ready.
fn serials() -> Vec<String> {
    let out = adb_output(&["devices"]).unwrap_or_default();
    out.lines()
        .skip(1)
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let serial = words.next()?;
            (words.next() == Some("device")).then(|| serial.to_string())
        })
        .collect()
}

static SERIALS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// The serial for a device: itself, or the running emulator of an
/// `avd:NAME` (remembered while it answers). None while it isn't running.
pub fn resolve(target: &str) -> Option<String> {
    let Some(avd) = target.strip_prefix("avd:") else { return Some(target.to_string()) };
    let cache = SERIALS.get_or_init(Default::default);
    let ready = serials();
    if let Some(serial) = cache.lock().ok()?.get(target).filter(|s| ready.contains(s)).cloned() {
        return Some(serial);
    }
    // The emulator's own file says which console port it has; else ask
    // each running emulator its name over its console.
    let listed = discovery_files().iter().find_map(|ini| {
        let port = ini_value(ini, "port.serial")?;
        (ini_value(ini, "avd.id")? == avd).then(|| format!("emulator-{port}"))
    });
    let serial = match listed.filter(|s| ready.contains(s)) {
        Some(serial) => serial,
        None => ready.into_iter().filter(|s| s.starts_with("emulator-")).find(|s| {
            adb_output(&["-s", s, "emu", "avd", "name"]).is_some_and(|o| o.lines().next().map(str::trim) == Some(avd))
        })?,
    };
    cache.lock().ok()?.insert(target.to_string(), serial.clone());
    Some(serial)
}

/// The screen's size in pixels, as input coordinates count them (an
/// override set with `wm size` wins).
pub fn screen_size(serial: &str) -> Option<(u32, u32)> {
    parse_wm_size(&adb_output(&["-s", serial, "shell", "wm", "size"])?)
}

fn parse_wm_size(out: &str) -> Option<(u32, u32)> {
    let size = |prefix: &str| {
        let line = out.lines().find(|l| l.trim_start().starts_with(prefix))?;
        let (w, h) = line.rsplit(' ').next()?.trim().split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    };
    size("Override size:").or_else(|| size("Physical size:"))
}

// ---- iOS simulators

/// idb (https://fbidb.io): on PATH, else where `uv tool install fb-idb` or
/// pipx put it.
pub fn idb() -> String {
    if crate::packages::on_path("idb") {
        return "idb".into();
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".local/bin/idb"))
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "idb".into())
}

pub fn idb_installed() -> bool {
    idb() != "idb" || crate::packages::on_path("idb")
}

/// What to install for iOS screens and input.
pub const IDB_HINT: &str = "needs idb: brew install facebook/fb/idb-companion and uv tool install fb-idb";

static IOS_SIZES: OnceLock<Mutex<HashMap<String, (u32, u32)>>> = OnceLock::new();

/// A simulator's screen in points (what idb's taps count in), once it's
/// booted (it's booted if it isn't). None while it's starting.
pub fn ios_screen(udid: &str) -> Option<(u32, u32)> {
    let cache = IOS_SIZES.get_or_init(Default::default);
    if let Some(size) = cache.lock().ok()?.get(udid).copied() {
        return Some(size);
    }
    let out = Command::new(idb()).args(["describe", "--udid", udid, "--json"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let (state, size) = parse_describe(&String::from_utf8_lossy(&out.stdout))?;
    if state != "Booted" {
        let _ = Command::new("xcrun").args(["simctl", "boot", udid]).stdin(Stdio::null()).stderr(Stdio::null()).status();
        return None;
    }
    cache.lock().ok()?.insert(udid.to_string(), size);
    Some(size)
}

/// idb describe --json: the state and the screen in points.
fn parse_describe(json: &str) -> Option<(String, (u32, u32))> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let screen = &v["screen_dimensions"];
    let size = (screen["width_points"].as_u64()? as u32, screen["height_points"].as_u64()? as u32);
    Some((v["state"].as_str().unwrap_or("Booted").to_string(), size))
}

// ---- Input

/// A device input goes to, found from its target.
#[derive(Debug, Clone, PartialEq)]
pub enum Device {
    Android(String),
    Ios(String),
}

pub fn locate(target: &str) -> Option<Device> {
    match target.strip_prefix("ios:") {
        Some(udid) => Some(Device::Ios(udid.to_string())),
        None => resolve(target).map(Device::Android),
    }
}

static SHELLS: OnceLock<Mutex<HashMap<String, (Child, ChildStdin)>>> = OnceLock::new();

/// Runs a command in the device's shell (a local one for a simulator),
/// without waiting for it. Commands run one after another, in the order
/// sent. False when there's no device.
pub fn shell_send(target: &str, command: &str) -> bool {
    let Some(device) = locate(target) else { return false };
    let (key, mut start) = match &device {
        Device::Android(serial) => {
            let mut c = Command::new(adb());
            c.args(["-s", serial, "shell"]);
            (serial.clone(), c)
        }
        Device::Ios(udid) => (format!("ios:{udid}"), Command::new("/bin/sh")),
    };
    let Ok(mut shells) = SHELLS.get_or_init(Default::default).lock() else { return false };
    for _ in 0..2 {
        let alive = shells.get_mut(&key).is_some_and(|(child, _)| matches!(child.try_wait(), Ok(None)));
        if !alive {
            let started = start.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
            let Ok(mut child) = started else { return false };
            let Some(stdin) = child.stdin.take() else { return false };
            shells.insert(key.clone(), (child, stdin));
        }
        let Some((_, stdin)) = shells.get_mut(&key) else { return false };
        if writeln!(stdin, "{command}").and_then(|_| stdin.flush()).is_ok() {
            return true;
        }
        shells.remove(&key);
    }
    false
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// `input text` for some text: quoted for the shell, spaces as %s (input
/// text ends at a space otherwise).
pub fn text_command(text: &str) -> String {
    format!("input text {}", quote(&text.replace('%', "\\%").replace(' ', "%s")))
}

/// `idb ui ARGS` for a simulator.
fn idb_ui(udid: &str, args: &str) -> String {
    format!("{} ui {args} --udid {}", quote(&idb()), quote(udid))
}

/// A tap, a long press (held still) or a swipe, from where the mouse went
/// down to where it came up, in the device's input units (Android's
/// pixels, iOS's points).
pub fn gesture_command(device: &Device, from: (f32, f32), to: (f32, f32), held_ms: u64) -> String {
    let moved = ((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt();
    let still = match device {
        Device::Android(_) => moved < 12.,
        Device::Ios(_) => moved < 5.,
    };
    let (x1, y1, x2, y2) = (from.0 as i32, from.1 as i32, to.0 as i32, to.1 as i32);
    let secs = |ms: u64| format!("{:.2}", ms as f32 / 1000.);
    match device {
        Device::Android(_) if still && held_ms >= 450 => format!("input swipe {x1} {y1} {x1} {y1} {}", held_ms.min(3000)),
        Device::Android(_) if still => format!("input tap {x1} {y1}"),
        Device::Android(_) => format!("input swipe {x1} {y1} {x2} {y2} {}", held_ms.clamp(60, 2000)),
        Device::Ios(udid) if still && held_ms >= 450 => idb_ui(udid, &format!("tap --duration {} {x1} {y1}", secs(held_ms.min(3000)))),
        Device::Ios(udid) if still => idb_ui(udid, &format!("tap {x1} {y1}")),
        Device::Ios(udid) => idb_ui(udid, &format!("swipe --duration {} {x1} {y1} {x2} {y2}", secs(held_ms.clamp(60, 2000)))),
    }
}

/// A swipe for the wheel: from the middle of the screen, `reach` up or down.
pub fn wheel_command(device: &Device, (w, h): (u32, u32), reach: f32) -> String {
    let (x, y) = ((w / 2) as i32, (h / 2) as i32);
    let to = (h as f32 / 2. + reach) as i32;
    match device {
        Device::Android(_) => format!("input swipe {x} {y} {x} {to} 120"),
        Device::Ios(udid) => idb_ui(udid, &format!("swipe --duration 0.12 {x} {y} {x} {to}")),
    }
}

/// A key typed into the device: Vim's name for it, and the text it types.
pub fn key_command(device: &Device, key: &str, text: &str) -> Result<String, String> {
    // Android's key codes, then the USB HID ones idb takes.
    let codes = match key {
        "RET" => Some((66, 40)),
        "DEL" => Some((67, 42)),
        "TAB" => Some((61, 43)),
        "<left>" => Some((21, 80)),
        "<right>" => Some((22, 79)),
        "<up>" => Some((19, 82)),
        "<down>" => Some((20, 81)),
        "<delete>" => Some((112, 76)),
        _ => None,
    };
    Ok(match (device, codes) {
        (Device::Android(_), Some((code, _))) => format!("input keyevent {code}"),
        (Device::Ios(udid), Some((_, code))) => idb_ui(udid, &format!("key {code}")),
        (_, None) if text.is_empty() => return Err(format!("{key} isn't sent to the device")),
        (Device::Android(_), None) => text_command(text),
        (Device::Ios(udid), None) => idb_ui(udid, &format!("text {}", quote(text))),
    })
}

/// A button: back, home, recents, power, volume-up, volume-down. iOS has no
/// back button: a swipe in from the left edge goes back, as in most apps.
pub fn button_command(device: &Device, button: &str) -> Result<String, String> {
    let android = |code: u32| Ok(format!("input keyevent {code}"));
    match (device, button) {
        (Device::Android(_), "back") => android(4),
        (Device::Android(_), "home") => android(3),
        (Device::Android(_), "recents") => android(187),
        (Device::Android(_), "power") => android(26),
        (Device::Android(_), "volume-up") => android(24),
        (Device::Android(_), "volume-down") => android(25),
        (Device::Ios(udid), "home") => Ok(idb_ui(udid, "button HOME")),
        (Device::Ios(udid), "power") => Ok(idb_ui(udid, "button LOCK")),
        (Device::Ios(udid), "back") => {
            let (w, h) = ios_screen(udid).ok_or("the simulator isn't running")?;
            Ok(idb_ui(udid, &format!("swipe --duration 0.25 1 {} {} {}", h / 2, w * 3 / 4, h / 2)))
        }
        (Device::Ios(_), _) => Err(format!("the iOS simulator has no {button} button")),
        _ => Err(format!("no button named {button}")),
    }
}

// ---- The emulator console

/// An emulator's console port: its serial's number.
fn console_port(serial: &str) -> Option<u16> {
    serial.strip_prefix("emulator-")?.parse().ok()
}

static CONSOLES: OnceLock<Mutex<HashMap<String, std::io::BufReader<std::net::TcpStream>>>> = OnceLock::new();

/// Reads the console's answer: true for OK, false for KO (or nothing).
fn console_reply(reader: &mut std::io::BufReader<std::net::TcpStream>) -> std::io::Result<bool> {
    use std::io::BufRead as _;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let line = line.trim();
        if line == "OK" {
            return Ok(true);
        }
        if line.starts_with("KO") {
            return Ok(false);
        }
    }
}

/// Connects to an emulator's console, signed in with the token the
/// emulator keeps in ~/.emulator_console_auth_token.
fn console_connect(port: u16) -> std::io::Result<std::io::BufReader<std::net::TcpStream>> {
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let stream = std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300))?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
    stream.set_nodelay(true)?;
    let mut reader = std::io::BufReader::new(stream);
    console_reply(&mut reader)?;
    let token = std::env::var_os("HOME")
        .and_then(|h| std::fs::read_to_string(std::path::PathBuf::from(h).join(".emulator_console_auth_token")).ok())
        .unwrap_or_default();
    if !token.trim().is_empty() {
        writeln!(reader.get_mut(), "auth {}", token.trim())?;
        if !console_reply(&mut reader)? {
            return Err(std::io::Error::other("the console didn't take the token"));
        }
    }
    Ok(reader)
}

/// Sends console commands to an emulator: false when it has no console,
/// it can't be reached, or a command is refused.
pub fn console_send(serial: &str, commands: &[String]) -> bool {
    let Some(port) = console_port(serial) else { return false };
    let Ok(mut consoles) = CONSOLES.get_or_init(Default::default).lock() else { return false };
    for _ in 0..2 {
        if !consoles.contains_key(serial) {
            match console_connect(port) {
                Ok(reader) => consoles.insert(serial.to_string(), reader),
                Err(_) => return false,
            };
        }
        let Some(reader) = consoles.get_mut(serial) else { return false };
        let sent = commands.iter().try_fold(true, |ok, c| {
            writeln!(reader.get_mut(), "{c}")?;
            Ok::<_, std::io::Error>(ok && console_reply(reader)?)
        });
        match sent {
            Ok(ok) => return ok,
            // A console that went away (the emulator restarted): once more.
            Err(_) => consoles.remove(serial),
        };
    }
    false
}

/// Text typed into an emulator, as a console command: plain ASCII only
/// (anything else goes through input).
fn console_text_command(text: &str) -> Option<String> {
    (!text.is_empty() && text.chars().all(|c| c.is_ascii_graphic() || c == ' ')).then(|| format!("event text {text}"))
}

/// The Linux key code an emulator's keyboard (qwerty2) maps to a key or
/// button: 102 is its home button, not KEY_HOMEPAGE.
fn evdev_code(name: &str) -> Option<u32> {
    Some(match name {
        "RET" => 28,
        "DEL" => 14,
        "TAB" => 15,
        "<up>" => 103,
        "<left>" => 105,
        "<right>" => 106,
        "<down>" => 108,
        "<delete>" => 111,
        "back" => 158,
        "home" => 102,
        "recents" => 580,
        "power" => 116,
        "volume-up" => 115,
        "volume-down" => 114,
        _ => return None,
    })
}

/// The files running emulators write about themselves (avd/running/
/// pid_N.ini): their AVD, ports and gRPC token.
fn discovery_files() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return Vec::new() };
    let mut dirs = vec![home.join("Library/Caches/TemporaryItems/avd/running")];
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        dirs.push(std::path::PathBuf::from(runtime).join("avd/running"));
    }
    if let Some(user) = std::env::var_os("USER") {
        dirs.push(std::env::temp_dir().join(format!("android-{}", user.to_string_lossy())).join("avd/running"));
    }
    dirs.iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".ini"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect()
}

fn ini_value<'a>(ini: &'a str, key: &str) -> Option<&'a str> {
    ini.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::trim))
}

/// Where a running emulator's gRPC service is, and its token, found by its
/// console port.
fn grpc_endpoint(serial: &str) -> Option<(u16, String)> {
    let console = console_port(serial)?;
    discovery_files().iter().find_map(|ini| parse_discovery(ini, console))
}

fn parse_discovery(ini: &str, console: u16) -> Option<(u16, String)> {
    if ini_value(ini, "port.serial")?.parse::<u16>().ok()? != console {
        return None;
    }
    Some((ini_value(ini, "grpc.port")?.parse().ok()?, ini_value(ini, "grpc.token").unwrap_or_default().to_string()))
}

/// A KeyboardEvent (emulator_controller.proto) pressing a key by its Linux
/// code, framed as a gRPC message.
fn key_press_message(code: u32) -> Vec<u8> {
    // codeType = Evdev (1), eventType = keypress (2), keyCode.
    let mut msg = vec![0x08, 0x01, 0x10, 0x02, 0x18];
    let mut n = code;
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        msg.push(if n == 0 { byte } else { byte | 0x80 });
        if n == 0 {
            break;
        }
    }
    let mut framed = vec![0];
    framed.extend((msg.len() as u32).to_be_bytes());
    framed.extend(msg);
    framed
}

/// Presses a key on an emulator through its gRPC service: false when it
/// has none, or the call fails.
fn grpc_key(serial: &str, code: u32) -> bool {
    use std::io::Read as _;
    let Some((port, token)) = grpc_endpoint(serial) else { return false };
    // The token goes in a file of our own rather than where ps shows it.
    let header = std::env::temp_dir().join(format!("vmac-emulator-{port}.header"));
    let written = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&header)
            .and_then(|mut f| f.write_all(format!("authorization: Bearer {token}\n").as_bytes()))
    };
    if written.is_err() {
        return false;
    }
    let url = format!("http://127.0.0.1:{port}/android.emulation.control.EmulatorController/sendKey");
    let child = Command::new("curl")
        .args(["-s", "-m", "2", "--http2-prior-knowledge", "-X", "POST", "-H", "content-type: application/grpc", "-H", "te: trailers"])
        .args(["-H", &format!("@{}", header.display()), "--data-binary", "@-", "-D", "-", "-o", "/dev/null", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&key_press_message(code));
    }
    let mut headers = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut headers);
    }
    let _ = child.wait();
    headers.lines().any(|l| l.trim().eq_ignore_ascii_case("grpc-status: 0"))
}

// ---- A simulator's HID stream

/// idb_companion's socket for a simulator (the idb command starts it).
fn companion_socket(udid: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/idb/{udid}_companion.sock"))
}

static HID_STREAMS: OnceLock<Mutex<HashMap<String, (Child, ChildStdin)>>> = OnceLock::new();

/// Sends HID events (idb.proto's HIDEvent, framed) to a simulator, over
/// one call to the companion's hid kept open: a curl streaming them as
/// they're written. The companion acts on an event once the next one
/// arrives, so each is followed by a delay of nothing, which pushes it
/// through (else a tap's touch would go down only as it comes up, too
/// short to count). False when the companion isn't running.
fn hid_send(udid: &str, events: &[Vec<u8>]) -> bool {
    let socket = companion_socket(udid);
    if !socket.exists() {
        return false;
    }
    let Ok(mut streams) = HID_STREAMS.get_or_init(Default::default).lock() else { return false };
    for _ in 0..2 {
        let alive = streams.get_mut(udid).is_some_and(|(child, _)| matches!(child.try_wait(), Ok(None)));
        if !alive {
            let started = Command::new("curl")
                .args(["-s", "-N", "--http2-prior-knowledge", "--unix-socket"])
                .arg(&socket)
                .args(["-X", "POST", "-T", "-", "-H", "content-type: application/grpc", "-H", "te: trailers"])
                .args(["-o", "/dev/null", "http://localhost/idb.CompanionService/hid"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = started else { return false };
            let Some(stdin) = child.stdin.take() else { return false };
            streams.insert(udid.to_string(), (child, stdin));
        }
        let Some((_, stdin)) = streams.get_mut(udid) else { return false };
        let push = hid_delay(0.);
        let written = events.iter().try_for_each(|e| stdin.write_all(e).and_then(|_| stdin.write_all(&push))).and_then(|_| stdin.flush());
        if written.is_ok() {
            return true;
        }
        streams.remove(udid);
    }
    false
}

/// Protobuf: a number as a varint, seven bits a byte.
fn pb_varint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        out.push(if n == 0 { byte } else { byte | 0x80 });
        if n == 0 {
            return;
        }
    }
}

/// Protobuf: a length-delimited field.
fn pb_bytes(field: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![field << 3 | 2];
    pb_varint(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    out
}

/// Protobuf: a varint field.
fn pb_number(field: u8, n: u64) -> Vec<u8> {
    let mut out = vec![field << 3];
    pb_varint(&mut out, n);
    out
}

/// A HIDEvent pressing (or releasing) an action, framed for gRPC.
fn hid_press(action: Vec<u8>, up: bool) -> Vec<u8> {
    let mut press = pb_bytes(1, &action);
    press.extend([0x10, up as u8]);
    let event = pb_bytes(1, &press);
    let mut framed = vec![0];
    framed.extend((event.len() as u32).to_be_bytes());
    framed.extend(event);
    framed
}

/// A HIDEvent waiting `secs`, framed.
fn hid_delay(secs: f64) -> Vec<u8> {
    let mut body = vec![0x09];
    body.extend(secs.to_le_bytes());
    let event = pb_bytes(3, &body);
    let mut framed = vec![0];
    framed.extend((event.len() as u32).to_be_bytes());
    framed.extend(event);
    framed
}

/// A finger at a point, in points: down, or moved there while down, or up.
fn hid_touch((x, y): (f32, f32), up: bool) -> Vec<u8> {
    let mut point = vec![0x09];
    point.extend((x as f64).to_le_bytes());
    point.push(0x11);
    point.extend((y as f64).to_le_bytes());
    hid_press(pb_bytes(1, &pb_bytes(1, &point)), up)
}

/// A key (a USB HID usage) going down or up.
fn hid_key(code: u8, up: bool) -> Vec<u8> {
    hid_press(pb_bytes(3, &pb_number(1, code as u64)), up)
}

/// A button (idb's HIDButtonType) going down or up.
fn hid_button(button: u8, up: bool) -> Vec<u8> {
    hid_press(pb_bytes(2, &pb_number(1, button as u64)), up)
}

/// The keys that type a character on a US keyboard: its usage, and whether
/// shift is held.
fn hid_char(c: char) -> Option<(u8, bool)> {
    const SHIFTED: &str = ")!@#$%^&*(";
    Some(match c {
        'a'..='z' => (4 + (c as u8 - b'a'), false),
        'A'..='Z' => (4 + (c as u8 - b'A'), true),
        '1'..='9' => (30 + (c as u8 - b'1'), false),
        '0' => (39, false),
        ' ' => (44, false),
        '\n' => (40, false),
        '\t' => (43, false),
        _ => {
            if let Some(i) = SHIFTED.find(c) {
                return Some((if i == 0 { 39 } else { 29 + i as u8 }, true));
            }
            let (plain, shifted) = (r"-=[]\;'`,./", r#"_+{}|:"~<>?"#);
            let i = plain.find(c).map(|i| (i, false)).or_else(|| shifted.find(c).map(|i| (i, true)))?;
            ([45, 46, 47, 48, 49, 51, 52, 53, 54, 55, 56][i.0], i.1)
        }
    })
}

/// The events typing some text: None if a character has no key.
fn hid_text(text: &str) -> Option<Vec<Vec<u8>>> {
    const SHIFT: u8 = 225;
    let mut events = Vec::new();
    for c in text.chars() {
        let (code, shift) = hid_char(c)?;
        if shift {
            events.push(hid_key(SHIFT, false));
        }
        events.extend([hid_key(code, false), hid_key(code, true)]);
        if shift {
            events.push(hid_key(SHIFT, true));
        }
    }
    Some(events)
}

/// The usage for a key typed by name (RET, DEL...).
fn hid_named_key(key: &str) -> Option<u8> {
    Some(match key {
        "RET" => 40,
        "DEL" => 42,
        "TAB" => 43,
        "<right>" => 79,
        "<left>" => 80,
        "<down>" => 81,
        "<up>" => 82,
        "<delete>" => 76,
        _ => return None,
    })
}

/// idb's HIDButtonType for a button.
fn hid_button_type(name: &str) -> Option<u8> {
    Some(match name {
        "home" => 1,
        "power" => 2,
        "volume-up" => 6,
        "volume-down" => 7,
        _ => return None,
    })
}

// ---- Live touches: an emulator's console, a simulator's HID stream

/// A touch on the screen, in the device's input units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Touch {
    Down,
    Move,
    Up,
}

fn touch_command(touch: Touch, (x, y): (f32, f32)) -> String {
    let pressed = if touch == Touch::Up { 0 } else { 1 };
    format!("event mouse {} {} 0 {pressed}", x as i32, y as i32)
}

/// Touches the screen as the mouse moves: an emulator through its
/// console, a simulator through its HID stream (a move is the finger down
/// at the new point). False for a device without either (then a gesture
/// goes on mouse up).
pub fn touch(device: &Device, touch: Touch, at: (f32, f32)) -> bool {
    match device {
        Device::Android(serial) => console_send(serial, &[touch_command(touch, at)]),
        Device::Ios(udid) => hid_send(udid, &[hid_touch(at, touch == Touch::Up)]),
    }
}

/// A swipe made of live touches, on a thread: down, along to `to` in a
/// few steps over `ms`, up. False when the device takes no live touches.
fn live_swipe(device: &Device, from: (f32, f32), to: (f32, f32), ms: u64) -> bool {
    if !touch(device, Touch::Down, from) {
        return false;
    }
    let device = device.clone();
    std::thread::spawn(move || {
        const STEPS: u64 = 6;
        for i in 1..=STEPS {
            std::thread::sleep(std::time::Duration::from_millis(ms / STEPS));
            let f = i as f32 / STEPS as f32;
            touch(&device, Touch::Move, (from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f));
        }
        touch(&device, Touch::Up, to);
    });
    true
}

/// Scrolls by `reach` (input units, down positive): a live swipe from the
/// middle, else a swipe command.
pub fn wheel(target: &str, device: &Device, (w, h): (u32, u32), reach: f32) -> bool {
    let middle = (w as f32 / 2., h as f32 / 2.);
    live_swipe(device, middle, (middle.0, middle.1 + reach), 100)
        || shell_send(target, &wheel_command(device, (w, h), reach))
}

/// Types a key into the device. An emulator: a key through gRPC, text
/// through the console. A simulator: through its HID stream. Else through
/// the shell.
pub fn key(target: &str, key: &str, text: &str) -> Result<(), String> {
    let fast = match locate(target) {
        Some(Device::Android(serial)) => match evdev_code(key) {
            Some(code) => grpc_key(&serial, code),
            None => console_text_command(text).is_some_and(|c| console_send(&serial, &[c])),
        },
        Some(Device::Ios(udid)) => {
            let events = match hid_named_key(key) {
                Some(code) => Some(vec![hid_key(code, false), hid_key(code, true)]),
                None if text.is_empty() => None,
                None => hid_text(text),
            };
            events.is_some_and(|e| hid_send(&udid, &e))
        }
        None => false,
    };
    if fast { Ok(()) } else { send(target, |d| key_command(d, key, text)) }
}

/// Presses a button. An emulator: through gRPC. A simulator: through its
/// HID stream (back is a swipe in from the left edge). Else the shell.
pub fn button(target: &str, name: &str) -> Result<(), String> {
    let fast = match locate(target) {
        Some(Device::Android(serial)) => evdev_code(name).is_some_and(|code| grpc_key(&serial, code)),
        Some(Device::Ios(udid)) if name == "back" => ios_screen(&udid).is_some_and(|(w, h)| {
            let y = h as f32 / 2.;
            live_swipe(&Device::Ios(udid.clone()), (1., y), (w as f32 * 0.75, y), 250)
        }),
        Some(Device::Ios(udid)) => {
            hid_button_type(name).is_some_and(|b| hid_send(&udid, &[hid_button(b, false), hid_button(b, true)]))
        }
        None => false,
    };
    if fast { Ok(()) } else { send(target, |d| button_command(d, name)) }
}

/// Sends a key or a button, saying why not when it can't.
pub fn send(target: &str, command: impl FnOnce(&Device) -> Result<String, String>) -> Result<(), String> {
    let device = locate(target).ok_or("the device isn't connected")?;
    if matches!(device, Device::Ios(_)) && !idb_installed() {
        return Err(format!("iOS input {IDB_HINT}"));
    }
    let command = command(&device)?;
    if shell_send(target, &command) { Ok(()) } else { Err("the device isn't connected".into()) }
}

// ---- The H.264 stream

/// Splits an Annex B byte stream (00 00 01 or 00 00 00 01 before each
/// unit) into whole NAL units as bytes arrive, each with its start code.
#[derive(Default)]
pub struct NalSplitter {
    pending: Vec<u8>,
}

fn start_codes(bytes: &[u8]) -> Vec<usize> {
    let mut at = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            // A 4-byte code's leading zero belongs to it.
            at.push(if i > 0 && bytes[i - 1] == 0 { i - 1 } else { i });
            i += 3;
        } else {
            i += 1;
        }
    }
    at
}

impl NalSplitter {
    /// The units completed by these bytes (the last one waits for the next
    /// start code).
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        let starts = start_codes(&self.pending);
        if starts.len() < 2 {
            return Vec::new();
        }
        let units = starts.windows(2).map(|w| self.pending[w[0]..w[1]].to_vec()).collect();
        self.pending.drain(..starts[starts.len() - 1]);
        units
    }

    /// The unit still waiting, taken as whole: the stream paused after it
    /// (screenrecord sends nothing more while the screen is still).
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        let start = *start_codes(&self.pending).first()?;
        let unit = self.pending.split_off(start);
        self.pending.clear();
        (unit.len() > 4).then_some(unit)
    }
}

/// The size to stream at: no wider than `max_width`, the shape kept, both
/// even (the encoder needs it).
pub fn stream_size((w, h): (u32, u32), max_width: u32) -> (u32, u32) {
    let even = |n: u32| (n / 2 * 2).max(2);
    if w <= max_width {
        return (even(w), even(h));
    }
    (even(max_width), even((h as u64 * max_width as u64 / w.max(1) as u64) as u32))
}

#[cfg(feature = "gui")]
pub use view::DeviceView;

#[cfg(feature = "gui")]
mod view {
    use super::*;
    use crate::frame::WindowId;
    use gpui_kit::component::ActiveTheme as _;
    use gpui_kit::{
        Bounds, Context, DispatchPhase, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
        Pixels, Render, RenderImage, ScrollWheelEvent, Styled as _, Window, canvas, div, img, px,
    };
    use std::cell::Cell;
    use std::io::Read as _;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
    use std::time::{Duration, Instant};

    /// How wide the screen is streamed, in pixels: sharp in a window, light
    /// to decode.
    const STREAM_WIDTH: u32 = 720;
    /// How often decoded frames are looked for.
    const FRAME_POLL: Duration = Duration::from_millis(16);
    const MARGIN: f32 = 8.;
    /// A pause in the stream this long ends the unit being read.
    const STREAM_PAUSE: Duration = Duration::from_millis(10);

    enum Update {
        Status(String),
        /// The device and its screen's size in input units, once known.
        Screen(Device, (u32, u32)),
        /// A frame: width, height, BGRA pixels.
        Frame(u32, u32, Vec<u8>),
    }

    /// Streams the screen until the view goes (its receiver with it):
    /// waits for the device, restarts screenrecord when it stops (it does
    /// every 3 minutes).
    fn stream(target: String, updates: Sender<Update>) {
        let wait = |why: String| {
            let gone = updates.send(Update::Status(why)).is_err();
            if !gone {
                std::thread::sleep(Duration::from_secs(1));
            }
            gone
        };
        loop {
            let Some(device) = locate(&target) else {
                if wait(format!("Waiting for {}…", target.trim_start_matches("avd:"))) {
                    return;
                }
                continue;
            };
            let (screen, mut command) = match &device {
                Device::Android(serial) => {
                    let Some(screen) = screen_size(serial) else {
                        if wait("Waiting for the device to start…".into()) {
                            return;
                        }
                        continue;
                    };
                    let (w, h) = stream_size(screen, STREAM_WIDTH);
                    let mut c = Command::new(adb());
                    c.args(["-s", serial, "exec-out", "screenrecord", "--output-format=h264", "--bit-rate", "8000000"])
                        .args(["--size", &format!("{w}x{h}"), "-"]);
                    (screen, c)
                }
                Device::Ios(udid) => {
                    if !idb_installed() {
                        let _ = updates.send(Update::Status(format!("The simulator's screen {IDB_HINT}")));
                        return;
                    }
                    let Some(points) = ios_screen(udid) else {
                        if wait("Waiting for the simulator to start…".into()) {
                            return;
                        }
                        continue;
                    };
                    // Points are a third (or half) of the pixels: about
                    // STREAM_WIDTH wide at this scale.
                    let scale = (STREAM_WIDTH as f32 / (points.0 as f32 * 3.)).min(1.);
                    let mut c = Command::new(idb());
                    c.args(["video-stream", "--udid", udid, "--format", "h264", "--fps", "30"])
                        .args(["--scale-factor", &format!("{scale:.2}")]);
                    (points, c)
                }
            };
            if updates.send(Update::Screen(device.clone(), screen)).is_err() {
                return;
            }
            let Ok(mut child) = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
                let _ = updates.send(Update::Status(format!("Can't run {:?}", command.get_program())));
                return;
            };
            let gone = decode(child.stdout.take(), &updates);
            let _ = child.kill();
            let _ = child.wait();
            if gone {
                return;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Decodes the stream until it ends (false) or the view is gone (true).
    fn decode(stdout: Option<std::process::ChildStdout>, updates: &Sender<Update>) -> bool {
        use openh264::formats::YUVSource as _;
        let Some(mut stdout) = stdout else { return false };
        let Ok(mut decoder) = openh264::decoder::Decoder::new() else {
            let _ = updates.send(Update::Status("No H.264 decoder".into()));
            return true;
        };
        // Read on a thread of its own, so a pause in the stream shows here.
        let (chunks, arriving) = channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut chunk = vec![0u8; 1 << 16];
            while let Ok(n) = stdout.read(&mut chunk) {
                if n == 0 || chunks.send(chunk[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        let mut splitter = NalSplitter::default();
        loop {
            let mut units = match arriving.recv_timeout(STREAM_PAUSE) {
                Ok(bytes) => splitter.push(&bytes),
                // Paused: the last unit is whole.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => splitter.flush().into_iter().collect(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
            };
            if units.is_empty() {
                continue;
            }
            for unit in units.drain(..) {
                let Ok(Some(yuv)) = decoder.decode(&unit) else { continue };
                let (w, h) = yuv.dimensions();
                let mut pixels = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut pixels);
                for px in pixels.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                }
                if updates.send(Update::Frame(w as u32, h as u32, pixels)).is_err() {
                    return true;
                }
            }
        }
    }

    /// Where the screen is drawn and what's pressed on it.
    #[derive(Default)]
    struct Pointer {
        bounds: Cell<Option<Bounds<Pixels>>>,
        pressed: Cell<Option<((f32, f32), Instant)>>,
        /// The press went down on the device itself (an emulator's
        /// console): moves and the release follow it there.
        live: Cell<bool>,
        moved: Cell<Option<Instant>>,
        wheel: Cell<f32>,
        wheel_sent: Cell<Option<Instant>>,
    }

    /// A device's screen, fitted to the window; the mouse taps, swipes and
    /// scrolls on it.
    pub struct DeviceView {
        target: String,
        updates: Receiver<Update>,
        picture: Option<Arc<RenderImage>>,
        frame_size: Option<(f32, f32)>,
        /// The device input goes to and its screen's size, once known.
        device: Option<(Device, (u32, u32))>,
        status: String,
        pointer: Rc<Pointer>,
        window: WindowId,
    }

    impl DeviceView {
        pub fn new(target: String, cx: &mut Context<Self>) -> Self {
            let (send, updates) = channel();
            let streamed = target.clone();
            std::thread::spawn(move || stream(streamed, send));
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(FRAME_POLL).await;
                    if this.update(cx, |view, cx| view.take_updates(cx)).is_err() {
                        return;
                    }
                }
            })
            .detach();
            Self {
                target,
                updates,
                picture: None,
                frame_size: None,
                device: None,
                status: "Connecting…".into(),
                pointer: Rc::default(),
                window: 0,
            }
        }

        pub fn target(&self) -> &str {
            &self.target
        }

        pub fn set_window(&mut self, window: WindowId) {
            self.window = window;
        }

        fn take_updates(&mut self, cx: &mut Context<Self>) {
            let mut frame = None;
            let mut changed = false;
            loop {
                match self.updates.try_recv() {
                    Ok(Update::Frame(w, h, pixels)) => frame = Some((w, h, pixels)),
                    Ok(Update::Status(s)) => {
                        changed |= s != self.status;
                        self.status = s;
                    }
                    Ok(Update::Screen(device, size)) => {
                        self.device = Some((device, size));
                        self.status.clear();
                        changed = true;
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
            // Only the latest frame is worth drawing.
            if let Some((w, h, pixels)) = frame
                && let Some(buffer) = image::RgbaImage::from_raw(w, h, pixels)
            {
                self.frame_size = Some((w as f32, h as f32));
                if let Some(old) = self.picture.replace(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))) {
                    cx.drop_image(old, None);
                }
                changed = true;
            }
            if changed {
                cx.notify();
            }
        }
    }

    /// The screen's rectangle in a view's bounds: fitted, centered.
    fn fitted(bounds: Bounds<Pixels>, (w, h): (f32, f32)) -> (f32, f32, f32, f32) {
        let (bx, by): (f32, f32) = (bounds.origin.x.into(), bounds.origin.y.into());
        let (bw, bh): (f32, f32) = (bounds.size.width.into(), bounds.size.height.into());
        let scale = ((bw - 2. * MARGIN).max(1.) / w).min((bh - 2. * MARGIN).max(1.) / h);
        let (sw, sh) = (w * scale, h * scale);
        (bx + (bw - sw) / 2., by + (bh - sh) / 2., sw, sh)
    }

    /// The mouse on the screen: down, up and the wheel become gestures.
    fn mouse_catcher(pointer: Rc<Pointer>, frame: (f32, f32), device: (Device, (u32, u32))) -> impl IntoElement {
        let (device, (dw, dh)) = device;
        let serial = match &device {
            Device::Android(serial) => serial.clone(),
            Device::Ios(udid) => format!("ios:{udid}"),
        };
        let catch = pointer.clone();
        canvas(
            move |bounds, _, _| catch.bounds.set(Some(bounds)),
            move |bounds, _, window, _| {
                let (x0, y0, sw, sh) = fitted(bounds, frame);
                let to_device = move |p: gpui_kit::Point<Pixels>| {
                    let (x, y): (f32, f32) = (p.x.into(), p.y.into());
                    (((x - x0) / sw).clamp(0., 1.) * dw as f32, ((y - y0) / sh).clamp(0., 1.) * dh as f32)
                };
                let on_screen = move |p: gpui_kit::Point<Pixels>| {
                    let (x, y): (f32, f32) = (p.x.into(), p.y.into());
                    x >= x0 && x < x0 + sw && y >= y0 && y < y0 + sh
                };
                let down = pointer.clone();
                let down_device = device.clone();
                window.on_mouse_event(move |e: &MouseDownEvent, phase, _, _| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left && on_screen(e.position) {
                        let at = to_device(e.position);
                        down.pressed.set(Some((at, Instant::now())));
                        down.live.set(touch(&down_device, Touch::Down, at));
                    }
                });
                let moving = pointer.clone();
                let move_device = device.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, _| {
                    if phase != DispatchPhase::Bubble || !moving.live.get() || moving.pressed.get().is_none() {
                        return;
                    }
                    // A move a frame at most: enough to follow, few enough to keep up.
                    if moving.moved.get().is_some_and(|t| t.elapsed() < Duration::from_millis(8)) {
                        return;
                    }
                    moving.moved.set(Some(Instant::now()));
                    touch(&move_device, Touch::Move, to_device(e.position));
                });
                let up = pointer.clone();
                let up_serial = serial.clone();
                let up_device = device.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, _| {
                    if phase != DispatchPhase::Bubble || e.button != MouseButton::Left {
                        return;
                    }
                    if let Some((from, at)) = up.pressed.take() {
                        let to = to_device(e.position);
                        if up.live.replace(false) && touch(&up_device, Touch::Up, to) {
                            return;
                        }
                        let held = at.elapsed().as_millis() as u64;
                        shell_send(&up_serial, &gesture_command(&up_device, from, to, held));
                    }
                });
                let wheel = pointer.clone();
                let wheel_serial = serial.clone();
                window.on_mouse_event(move |e: &ScrollWheelEvent, phase, _, _| {
                    if phase != DispatchPhase::Bubble || !on_screen(e.position) || e.modifiers.control {
                        return;
                    }
                    let dy: f32 = e.delta.pixel_delta(px(20.)).y.into();
                    wheel.wheel.set(wheel.wheel.get() + dy);
                    // A swipe takes a moment: one per 150ms, the wheel added
                    // up meanwhile.
                    if wheel.wheel_sent.get().is_some_and(|t| t.elapsed() < Duration::from_millis(150)) {
                        return;
                    }
                    let total = wheel.wheel.replace(0.);
                    if total.abs() < 4. {
                        wheel.wheel.set(total);
                        return;
                    }
                    wheel.wheel_sent.set(Some(Instant::now()));
                    // Content follows the fingers: the wheel's pixels, on the device's scale.
                    let reach = (total * dh as f32 / sh).clamp(-(dh as f32) * 0.4, dh as f32 * 0.4);
                    super::wheel(&wheel_serial, &device, (dw, dh), reach);
                });
            },
        )
        .absolute()
        .size_full()
    }

    impl Render for DeviceView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = cx.theme();
            let muted = theme.muted_foreground;
            let base = div().flex_1().size_full().relative().overflow_hidden().bg(theme.muted);
            let (Some(picture), Some(frame)) = (self.picture.clone(), self.frame_size) else {
                let why = if self.status.is_empty() { "Waiting for the screen…".to_string() } else { self.status.clone() };
                return base.flex().items_center().justify_center().text_color(muted).child(why);
            };
            let bounds = self.pointer.bounds.get();
            let (w, h) = bounds.map_or(frame, |b| {
                let (_, _, w, h) = fitted(b, frame);
                (w, h)
            });
            base.flex()
                .items_center()
                .justify_center()
                .child(img(picture).flex_none().w(px(w)).h(px(h)).rounded(px(12.)))
                .children(self.device.clone().map(|d| mouse_catcher(self.pointer.clone(), frame, d)))
        }
    }

    #[cfg(test)]
    mod tests {
        /// Against a running emulator: VMAC_DEVICE=avd:NAME or a serial.
        #[test]
        #[ignore]
        fn the_screen_streams_and_decodes() {
            let target = std::env::var("VMAC_DEVICE").unwrap();
            let (send, updates) = super::channel();
            std::thread::spawn(move || super::stream(target, send));
            let start = super::Instant::now();
            let mut frames = 0;
            let mut size = None;
            while start.elapsed() < super::Duration::from_secs(8) {
                match updates.recv_timeout(super::Duration::from_millis(100)) {
                    Ok(super::Update::Frame(w, h, px)) => {
                        assert_eq!(px.len(), (w * h * 4) as usize);
                        if frames == 0 {
                            eprintln!("first frame {w}x{h} after {:?}", start.elapsed());
                            // Something moving, for more frames.
                            super::shell_send(&std::env::var("VMAC_DEVICE").unwrap(), "input swipe 600 2000 600 900 400");
                        }
                        frames += 1;
                        size = Some((w, h));
                    }
                    Ok(super::Update::Screen(device, s)) => eprintln!("{device:?}: {s:?}"),
                    Ok(super::Update::Status(s)) => eprintln!("status: {s}"),
                    Err(_) => {}
                }
            }
            eprintln!("{frames} frames in 8s, {size:?}");
            assert!(frames > 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a booted simulator: VMAC_SIMULATOR=UDID. Leaves screenshots
    /// of each step in VMAC_SHOTS.
    #[test]
    #[ignore]
    fn a_simulator_takes_touches_keys_and_buttons_fast() {
        let udid = std::env::var("VMAC_SIMULATOR").unwrap();
        let shots = std::env::var("VMAC_SHOTS").unwrap();
        let target = format!("ios:{udid}");
        let device = locate(&target).unwrap();
        let settle = || std::thread::sleep(std::time::Duration::from_millis(1500));
        let shot = |name: &str| {
            Command::new("xcrun").args(["simctl", "io", &udid, "screenshot", &format!("{shots}/{name}.png")]).output().unwrap();
        };
        assert!(ios_screen(&udid).is_some(), "booted, the companion running");
        Command::new("xcrun").args(["simctl", "launch", &udid, "com.apple.Preferences"]).output().unwrap();
        settle();
        let start = std::time::Instant::now();
        assert_eq!(button(&target, "home"), Ok(()));
        eprintln!("home: {:?}", start.elapsed());
        settle();
        shot("1-home");
        // Settings' icon on the home screen, then a 12-step drag.
        let start = std::time::Instant::now();
        assert!(touch(&device, Touch::Down, (339., 420.)));
        eprintln!("a touch down: {:?}", start.elapsed());
        // A click's length: a touch as short as nothing doesn't count.
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(touch(&device, Touch::Up, (339., 420.)));
        settle();
        shot("2-tapped-settings");
        // General, then back: a swipe in from the left edge.
        assert!(touch(&device, Touch::Down, (94., 318.)));
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(touch(&device, Touch::Up, (94., 318.)));
        settle();
        shot("2a-general");
        assert_eq!(button(&target, "back"), Ok(()));
        settle();
        shot("2b-back");
        assert_eq!(button(&target, "home"), Ok(()));
        settle();
        // Down from the middle: Search, as the mouse moves (a frame apart).
        let mut sending = std::time::Duration::ZERO;
        let mut timed = |f: &dyn Fn() -> bool| {
            let start = std::time::Instant::now();
            assert!(f());
            sending += start.elapsed();
            std::thread::sleep(std::time::Duration::from_millis(16));
        };
        timed(&|| touch(&device, Touch::Down, (201., 300.)));
        for i in 1..=10 {
            timed(&|| touch(&device, Touch::Move, (201., 300. + 25. * i as f32)));
        }
        timed(&|| touch(&device, Touch::Up, (201., 550.)));
        eprintln!("a 12-step drag, sending: {sending:?}");
        settle();
        let start = std::time::Instant::now();
        assert_eq!(key(&target, "x", "Hi there!"), Ok(()));
        assert_eq!(key(&target, "DEL", ""), Ok(()));
        eprintln!("text and a key: {:?}", start.elapsed());
        settle();
        shot("3-search-typed");
        assert_eq!(button(&target, "home"), Ok(()));
    }

    /// Against a running emulator: VMAC_DEVICE=avd:NAME or emulator-NNNN.
    #[test]
    #[ignore]
    fn an_emulator_takes_touches_keys_and_buttons_fast() {
        let target = std::env::var("VMAC_DEVICE").unwrap();
        let device = locate(&target).expect("the emulator is running");
        let start = std::time::Instant::now();
        assert!(touch(&device, Touch::Down, (672., 2400.)));
        for i in 1..=10 {
            assert!(touch(&device, Touch::Move, (672., 2400. - 120. * i as f32)));
        }
        assert!(touch(&device, Touch::Up, (672., 1200.)));
        eprintln!("a 12-step drag: {:?}", start.elapsed());
        let top = || {
            let out = Command::new(adb()).args(["shell", "dumpsys", "activity", "activities"]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.lines().find(|l| l.contains("topResumedActivity")).unwrap_or_default().to_string()
        };
        let settle = || std::thread::sleep(std::time::Duration::from_millis(1500));
        Command::new(adb()).args(["shell", "am", "start", "-W", "-a", "android.settings.SETTINGS"]).output().unwrap();
        settle();
        assert!(top().contains("settings"), "{}", top());
        let start = std::time::Instant::now();
        assert_eq!(button(&target, "home"), Ok(()));
        eprintln!("home: {:?}", start.elapsed());
        settle();
        assert!(top().contains("launcher"), "home went home: {}", top());
        let start = std::time::Instant::now();
        // Typed where it lands (the launcher's search, maybe): no RET, which
        // would open what it found.
        assert_eq!(key(&target, "x", "hi there"), Ok(()));
        assert_eq!(key(&target, "<left>", ""), Ok(()));
        assert_eq!(key(&target, "DEL", ""), Ok(()));
        assert_eq!(button(&target, "back"), Ok(()));
        assert_eq!(button(&target, "home"), Ok(()));
        eprintln!("text, two keys and two buttons: {:?}", start.elapsed());
    }

    #[test]
    fn nal_units_split_as_bytes_arrive() {
        let mut s = NalSplitter::default();
        assert!(s.push(&[0, 0, 0, 1, 0x67, 1, 2]).is_empty(), "the first unit waits for the next start");
        let units = s.push(&[3, 0, 0, 1, 0x68, 9, 0, 0, 0]);
        assert_eq!(units, vec![vec![0, 0, 0, 1, 0x67, 1, 2, 3]]);
        let units = s.push(&[1, 0x65, 7]);
        assert_eq!(units, vec![vec![0, 0, 1, 0x68, 9]], "a 4-byte start code split across reads");
        assert!(s.push(&[]).is_empty());
        assert_eq!(s.flush(), Some(vec![0, 0, 0, 1, 0x65, 7]), "a pause: the last unit is whole");
        assert_eq!(s.flush(), None);
    }

    #[test]
    fn sizes_and_gestures() {
        assert_eq!(parse_wm_size("Physical size: 1344x2992\n"), Some((1344, 2992)));
        assert_eq!(parse_wm_size("Physical size: 1344x2992\nOverride size: 1080x2400\n"), Some((1080, 2400)));
        assert_eq!(parse_wm_size("error"), None);
        assert_eq!(stream_size((1344, 2992), 720), (720, 1602));
        assert_eq!(stream_size((601, 801), 720), (600, 800));
        let android = Device::Android("emulator-5554".into());
        assert_eq!(gesture_command(&android, (100., 200.), (103., 201.), 80), "input tap 100 200");
        assert_eq!(gesture_command(&android, (100., 200.), (100., 200.), 900), "input swipe 100 200 100 200 900");
        assert_eq!(gesture_command(&android, (100., 900.), (100., 300.), 20), "input swipe 100 900 100 300 60");
        let ios = Device::Ios("U-1".into());
        let idb = quote(&idb());
        assert_eq!(gesture_command(&ios, (100., 200.), (101., 201.), 80), format!("{idb} ui tap 100 200 --udid 'U-1'"));
        assert_eq!(gesture_command(&ios, (100., 200.), (100., 200.), 900), format!("{idb} ui tap --duration 0.90 100 200 --udid 'U-1'"));
        assert_eq!(gesture_command(&ios, (100., 700.), (100., 200.), 300), format!("{idb} ui swipe --duration 0.30 100 700 100 200 --udid 'U-1'"));
        assert_eq!(key_command(&android, "RET", ""), Ok("input keyevent 66".into()));
        assert_eq!(key_command(&ios, "DEL", ""), Ok(format!("{idb} ui key 42 --udid 'U-1'")));
        assert_eq!(key_command(&ios, "x", "it's"), Ok(format!("{idb} ui text 'it'\\''s' --udid 'U-1'")));
        assert!(key_command(&ios, "C-x", "").is_err());
        assert_eq!(button_command(&android, "back"), Ok("input keyevent 4".into()));
        assert_eq!(button_command(&ios, "home"), Ok(format!("{idb} ui button HOME --udid 'U-1'")));
        assert!(button_command(&ios, "volume-up").is_err());
        assert_eq!(locate("ios:U-1"), Some(ios.clone()));
        assert_eq!(console_port("emulator-5556"), Some(5556));
        assert_eq!(console_port("R5CT1234"), None);
        assert_eq!(touch_command(Touch::Down, (672.4, 1500.)), "event mouse 672 1500 0 1");
        assert_eq!(touch_command(Touch::Up, (10., 20.)), "event mouse 10 20 0 0");
        assert_eq!(console_text_command("a b'").as_deref(), Some("event text a b'"));
        // HIDEvent { press { action { button { HOME } }, direction: UP } }, framed.
        assert_eq!(hid_button(1, true), [0, 0, 0, 0, 10, 0x0a, 8, 0x0a, 4, 0x12, 2, 0x08, 1, 0x10, 1]);
        // HIDEvent { press { action { touch { point { x: 1.0, y: 2.0 } } }, direction: DOWN } }.
        let mut point = vec![0x09];
        point.extend(1f64.to_le_bytes());
        point.push(0x11);
        point.extend(2f64.to_le_bytes());
        let press = [pb_bytes(1, &pb_bytes(1, &pb_bytes(1, &point))), vec![0x10, 0]].concat();
        let event = pb_bytes(1, &press);
        assert_eq!(hid_touch((1., 2.), false), [vec![0], (event.len() as u32).to_be_bytes().to_vec(), event].concat());
        // As idb's own client encodes a touch up at (339, 420).
        let idb_up = "0a1a0a160a140a12090000000000307540110000000000407a401001";
        let hex: String = hid_touch((339., 420.), true)[5..].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, idb_up);
        assert_eq!(hid_delay(0.), [0, 0, 0, 0, 11, 0x1a, 9, 0x09, 0, 0, 0, 0, 0, 0, 0, 0]);
        // Shift (225) down, as idb encodes it (but for the default
        // direction, which idb leaves out): the code is a two-byte varint.
        let hex: String = hid_key(225, false)[5..].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "0a090a051a0308e1011000");
        assert_eq!(hid_char('a'), Some((4, false)));
        assert_eq!(hid_char('Z'), Some((29, true)));
        assert_eq!(hid_char('!'), Some((30, true)));
        assert_eq!(hid_char(')'), Some((39, true)));
        assert_eq!(hid_char('?'), Some((56, true)));
        assert_eq!(hid_char('\''), Some((52, false)));
        assert_eq!(hid_char('é'), None);
        assert_eq!(hid_text("Hi").map(|e| e.len()), Some(6), "shift down, H down, up, shift up, i down, up");
        assert_eq!(hid_text("né"), None, "not typable: through idb ui text");
        assert_eq!(console_text_command("é"), None, "not ASCII: through input");
        assert_eq!(evdev_code("home"), Some(102));
        assert_eq!(evdev_code("x"), None);
        // KeyboardEvent { codeType: Evdev, eventType: keypress, keyCode: 580 }, framed.
        assert_eq!(key_press_message(580), [0, 0, 0, 0, 7, 0x08, 0x01, 0x10, 0x02, 0x18, 0xc4, 0x04]);
        let ini = "port.serial=5554\nport.adb=5555\ngrpc.port=8554\ngrpc.token=abc\n";
        assert_eq!(parse_discovery(ini, 5554), Some((8554, "abc".to_string())));
        assert_eq!(parse_discovery(ini, 5556), None, "another emulator's");
        let describe = r#"{"state": "Booted", "screen_dimensions": {"width": 1206, "height": 2622, "density": 3.0, "width_points": 402, "height_points": 874}}"#;
        assert_eq!(parse_describe(describe), Some(("Booted".into(), (402, 874))));
        assert_eq!(text_command("it's 100% done"), r"input text 'it'\''s%s100\%%sdone'");
        assert_eq!(resolve("emulator-5554").as_deref(), Some("emulator-5554"), "a serial is itself");
    }
}
