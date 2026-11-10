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
    let serial = ready.into_iter().filter(|s| s.starts_with("emulator-")).find(|s| {
        adb_output(&["-s", s, "emu", "avd", "name"]).is_some_and(|o| o.lines().next().map(str::trim) == Some(avd))
    })?;
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
        Bounds, Context, DispatchPhase, IntoElement, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement as _,
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
                window.on_mouse_event(move |e: &MouseDownEvent, phase, _, _| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left && on_screen(e.position) {
                        down.pressed.set(Some((to_device(e.position), Instant::now())));
                    }
                });
                let up = pointer.clone();
                let up_serial = serial.clone();
                let up_device = device.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, _| {
                    if phase != DispatchPhase::Bubble || e.button != MouseButton::Left {
                        return;
                    }
                    if let Some((from, at)) = up.pressed.take() {
                        let held = at.elapsed().as_millis() as u64;
                        shell_send(&up_serial, &gesture_command(&up_device, from, to_device(e.position), held));
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
                    // input takes a moment to start: one swipe per 150ms, the
                    // wheel added up meanwhile.
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
                    shell_send(&wheel_serial, &wheel_command(&device, (dw, dh), reach));
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
        let describe = r#"{"state": "Booted", "screen_dimensions": {"width": 1206, "height": 2622, "density": 3.0, "width_points": 402, "height_points": 874}}"#;
        assert_eq!(parse_describe(describe), Some(("Booted".into(), (402, 874))));
        assert_eq!(text_command("it's 100% done"), r"input text 'it'\''s%s100\%%sdone'");
        assert_eq!(resolve("emulator-5554").as_deref(), Some("emulator-5554"), "a serial is itself");
    }
}
