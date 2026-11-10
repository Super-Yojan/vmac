-- Mobile apps: build, run and read the logs of an iOS app (an Xcode project
-- or workspace, on a simulator) or an Android app (a Gradle project, on an
-- emulator or a plugged-in phone), from the project you're editing.
--
-- SPC r r   build, install and launch on the chosen device; the app's
--           output (print / NSLog, or logcat) follows in *run*
-- SPC r b   build only                 SPC r d   choose the device
-- SPC r s   choose the Xcode scheme    SPC r e   the last build's errors
-- SPC r k   stop the app               SPC r o   show *run*
-- SPC r L   set up sourcekit-lsp for the Xcode project (buildServer.json)
-- SPC r v   show the device's screen   SPC r Q   shut the emulator or simulator down
--
-- The device's screen (*device*, beside the code; it opens by itself when
-- an app runs): click to tap, drag to swipe, hold to long press, the wheel
-- scrolls. In it, i types into the device (ESC stops), b goes back (on iOS,
-- a swipe in from the left edge), H home, P presses power, + and - change
-- the volume; on Android, o shows the recent apps. With the screen in
-- vmac, the emulator or simulator runs without a window of its own: set
-- core.mobile.headless_emulator = false to keep it, or
-- core.mobile.show_screen = false not to open *device* on a run. An iOS
-- simulator's screen and input need idb (brew install
-- facebook/fb/idb-companion, then uv tool install fb-idb).
--
-- The first run asks for what it needs (a device, a scheme when there are
-- several) and remembers it for the project. A project with both an Xcode
-- project and a Gradle one (Kotlin Multiplatform) runs on whichever kind of
-- device you chose.
--
-- iOS needs Xcode; xcbeautify (brew install xcbeautify) tidies the build
-- output when it's installed, and xcode-build-server (brew install
-- xcode-build-server) lets sourcekit-lsp understand an Xcode project.
-- Android needs the Android SDK (ANDROID_HOME, else ~/Library/Android/sdk)
-- and a JDK: JAVA_HOME's, else Android Studio's own, as Android Studio
-- builds with it (a newer java on PATH can be too new for Gradle).
--
-- Builds are Debug; set core.mobile.configuration ("Release") or
-- core.mobile.android_variant ("Release", "FreeDebug", ...) to change it.

local V = vmac
local M = {}
core.mobile = M

M.run_buffer = "*run*"
M.configuration = "Debug"
M.android_variant = "Debug"
M.show_screen = true
M.headless_emulator = true
M.device_buffer = "*device*"

-- Programs (tests put stand-ins here).
M.xcrun = "xcrun"
M.xcodebuild = "xcodebuild"

local HOME = os.getenv("HOME") or ""

local function q(s) return "'" .. tostring(s):gsub("'", "'\\''") .. "'" end

local function cache_dir()
  if M.cache_path then return M.cache_path end
  local base = os.getenv("XDG_CACHE_HOME")
  if not base or base == "" then
    base = V.file_exists(HOME .. "/Library") and (HOME .. "/Library/Caches") or (HOME .. "/.cache")
  end
  local dir = base .. "/vmac/mobile"
  os.execute("mkdir -p " .. q(dir))
  return dir
end

local function data_file()
  if M.data_path then return M.data_path end
  local dir = os.getenv("VMAC_DATA_DIR")
  if not dir or dir == "" then
    local xdg = os.getenv("XDG_DATA_HOME")
    dir = ((xdg and xdg ~= "") and xdg or (HOME .. "/.local/share")) .. "/vmac"
  end
  return dir .. "/mobile.json"
end

local function read_file(path)
  local f = io.open(path, "r")
  if not f then return nil end
  local text = f:read("a")
  f:close()
  return text
end

local function write_file(path, text)
  local f = io.open(path, "w")
  if not f then return false end
  f:write(text)
  f:close()
  return true
end

-- A short name for a project, safe in a file name.
local function slug(root) return (root:gsub("^.*/", ""):gsub("[^%w%-_.]", "_")) end

---- What's remembered per project: { platform, ios = {...}, android = {...} }

local saved
local function settings_all()
  if not saved then
    local text = read_file(data_file())
    saved = text and V.json_decode(text) or {}
    if type(saved) ~= "table" then saved = {} end
  end
  return saved
end

function M.settings(root)
  local all = settings_all()
  all[root] = all[root] or {}
  return all[root]
end

function M.save()
  local path = data_file()
  os.execute("mkdir -p " .. q(path:gsub("/[^/]*$", "")))
  write_file(path, V.json_encode(settings_all()))
end

---- Running programs in the background

local jobs, next_job = {}, 0

-- Runs program in dir; done(ok, output, errors).
local function spawn(dir, program, args, done)
  next_job = next_job + 1
  local tag = "mobile:" .. next_job
  jobs[tag] = done
  V.spawn(dir, program, args, nil, tag)
end

local previous_spawn_done = core.events["spawn-done"]
core.events["spawn-done"] = function(args)
  local done = jobs[args[1]]
  if done then
    jobs[args[1]] = nil
    return done(args[2] == "ok", args[3], args[4])
  end
  if previous_spawn_done then return previous_spawn_done(args) end
end

local function last_line(text)
  local last = ""
  for line in (text or ""):gmatch("[^\n]+") do
    if line:match("%S") then last = line end
  end
  return last
end

---- The Android SDK

local function sdk()
  for _, var in ipairs({ "ANDROID_HOME", "ANDROID_SDK_ROOT" }) do
    local v = os.getenv(var)
    if v and v ~= "" then return v end
  end
  return HOME .. "/Library/Android/sdk"
end

local function sdk_tool(rel, name)
  local path = sdk() .. "/" .. rel
  if V.file_exists(path) then return path end
  return name
end

function M.adb() return M.adb_program or sdk_tool("platform-tools/adb", "adb") end
function M.emulator() return M.emulator_program or sdk_tool("emulator/emulator", "emulator") end

---- Finding the apps in a project

-- "a/b/App.xcodeproj/project.pbxproj" -> "a/b/App.xcodeproj".
local function xcode_containers(files)
  local workspaces, projects = {}, {}
  for _, f in ipairs(files) do
    local ws = f:match("^(.-%.xcworkspace)/contents%.xcworkspacedata$")
    if ws and not ws:find("%.xcodeproj/") then table.insert(workspaces, ws) end
    local proj = f:match("^(.-%.xcodeproj)/project%.pbxproj$")
    if proj then table.insert(projects, proj) end
  end
  local by_length = function(a, b) return #a < #b or (#a == #b and a < b) end
  table.sort(workspaces, by_length)
  table.sort(projects, by_length)
  local all = {}
  for _, w in ipairs(workspaces) do table.insert(all, { kind = "workspace", path = w }) end
  for _, p in ipairs(projects) do table.insert(all, { kind = "project", path = p }) end
  return all
end

-- The Android app module: the gradlew folder, the module's Gradle path
-- (":app") and its applicationId (from build.gradle(.kts), a guess the
-- built APK corrects).
local function android_app(root, files)
  local gradlew
  for _, f in ipairs(files) do
    if f == "gradlew" or f:match("/gradlew$") then
      if not gradlew or #f < #gradlew then gradlew = f end
    end
  end
  if not gradlew then return nil end
  local top = gradlew:match("^(.*)/gradlew$") or ""
  local best
  for _, f in ipairs(files) do
    local dir = f:match("^(.*)/build%.gradle%.kts$") or f:match("^(.*)/build%.gradle$")
    local inside = dir and (top == "" or dir:sub(1, #top + 1) == top .. "/")
    if inside then
      local text = read_file(root .. "/" .. f) or ""
      if text:find("android%.application") or text:find("com%.android%.application") then
        local module = top == "" and dir or dir:sub(#top + 2)
        local id = text:match("applicationId%s*=?%s*[\"']([^\"']+)") or text:match("namespace%s*=?%s*[\"']([^\"']+)")
        local candidate = { dir = top == "" and root or (root .. "/" .. top), module = ":" .. module:gsub("/", ":"),
          module_dir = root .. "/" .. dir, package = id }
        if not best or #module < #best.module then best = candidate end
      end
    end
  end
  return best
end

-- What can run from this project: { root, ios = {containers}, android = app }.
function M.project()
  local root = V.project_root()
  local files = V.project_files(root)
  local ios = xcode_containers(files)
  local project = { root = root, android = android_app(root, files) }
  if #ios > 0 then project.ios = ios end
  return project
end

---- Choosing an Xcode project and scheme

local function xcode_args(container)
  return { "-" .. container.kind, container.path }
end

local function with_container(project, then_)
  local s = M.settings(project.root)
  s.ios = s.ios or {}
  for _, c in ipairs(project.ios) do
    if s.ios.container and c.path == s.ios.container.path then return then_(c) end
  end
  if #project.ios == 1 then
    s.ios.container = project.ios[1]
    return then_(project.ios[1])
  end
  local names = {}
  for _, c in ipairs(project.ios) do table.insert(names, c.path) end
  core.pick("Xcode project", names, project.root, function(picked)
    for _, c in ipairs(project.ios) do
      if c.path == picked then
        s.ios.container = c
        s.ios.scheme = nil
        M.save()
        return then_(c)
      end
    end
  end)
end

-- Asks xcodebuild for the schemes; then_(scheme). force asks again.
local function with_scheme(project, force, then_)
  with_container(project, function(container)
    local s = M.settings(project.root).ios
    if s.scheme and not force then return then_(container, s.scheme) end
    V.message("Reading the schemes of " .. container.path .. "…")
    local args = xcode_args(container)
    table.insert(args, 1, "-json")
    table.insert(args, 1, "-list")
    spawn(project.root, M.xcodebuild, args, function(ok, out, err)
      if not ok then return V.message("xcodebuild: " .. last_line(err ~= "" and err or out)) end
      local value = V.json_decode(out:match("{.*}") or "") or {}
      local info = value.workspace or value.project or {}
      local schemes = info.schemes or {}
      if #schemes == 0 then return V.message("No schemes in " .. container.path) end
      local function chose(scheme)
        s.scheme = scheme
        M.save()
        then_(container, scheme)
      end
      if #schemes == 1 then return chose(schemes[1]) end
      core.pick("Scheme", schemes, "", chose)
    end)
  end)
end

---- Devices

-- runtime "com.apple.CoreSimulator.SimRuntime.iOS-26-0" -> "iOS 26.0".
local function runtime_name(key)
  local os_name, version = key:match("SimRuntime%.(%a+)%-([%d%-]+)$")
  if not os_name then return nil end
  return os_name .. " " .. version:gsub("%-", ".")
end

function M.ios_devices(done)
  spawn(HOME ~= "" and HOME or "/", M.xcrun, { "simctl", "list", "devices", "available", "-j" }, function(ok, out)
    local list = {}
    local value = ok and V.json_decode(out) or nil
    for runtime, devices in pairs((value and value.devices) or {}) do
      local name = runtime_name(runtime)
      if name and name:match("^iOS ") then
        for _, d in ipairs(devices) do
          table.insert(list, { platform = "ios", udid = d.udid, name = d.name, os = name,
            booted = d.state == "Booted" })
        end
      end
    end
    table.sort(list, function(a, b)
      if a.booted ~= b.booted then return a.booted end
      if a.os ~= b.os then return a.os > b.os end
      return a.name < b.name
    end)
    done(list)
  end)
end

function M.android_devices(done)
  local list, pending = {}, 2
  local function finish()
    pending = pending - 1
    if pending == 0 then done(list) end
  end
  spawn(HOME ~= "" and HOME or "/", M.emulator(), { "-list-avds" }, function(ok, out)
    if ok then
      for line in out:gmatch("[^\r\n]+") do
        if line:match("^[%w_.%-]+$") then table.insert(list, { platform = "android", avd = line, name = line }) end
      end
    end
    finish()
  end)
  spawn(HOME ~= "" and HOME or "/", M.adb(), { "devices", "-l" }, function(ok, out)
    if ok then
      for line in out:gmatch("[^\r\n]+") do
        local serial, rest = line:match("^(%S+)%s+device%s*(.*)$")
        if serial and not serial:match("^emulator%-") then
          local model = (rest:match("model:(%S+)") or serial):gsub("_", " ")
          table.insert(list, { platform = "android", serial = serial, name = model })
        end
      end
    end
    finish()
  end)
end

local function device_label(d)
  if d.platform == "ios" then
    return d.name .. "  ·  " .. d.os .. (d.booted and "  ·  booted" or "") .. "  (simulator)"
  elseif d.avd then
    return d.name .. "  (Android emulator)"
  end
  return d.name .. "  ·  " .. d.serial .. "  (Android device)"
end

-- Asks for a device among the platforms this project has; then_(device).
local function choose_device(project, then_)
  local wanted = {}
  if project.ios then table.insert(wanted, M.ios_devices) end
  if project.android then table.insert(wanted, M.android_devices) end
  if #wanted == 0 then return V.message("No Xcode or Android app in " .. project.root) end
  V.message("Looking for devices…")
  local all, pending = {}, #wanted
  for _, list in ipairs(wanted) do
    list(function(devices)
      for _, d in ipairs(devices) do table.insert(all, d) end
      pending = pending - 1
      if pending > 0 then return end
      if #all == 0 then
        return V.message(project.ios and "No simulators (Xcode › Settings › Components adds them)"
          or "No emulators or devices (Android Studio's Device Manager makes an emulator)")
      end
      local labels, by_label = {}, {}
      for _, d in ipairs(all) do
        local label = device_label(d)
        table.insert(labels, label)
        by_label[label] = d
      end
      core.pick("Run on", labels, "", function(label)
        local d = by_label[label]
        if not d then return end
        local s = M.settings(project.root)
        s.platform = d.platform
        s[d.platform] = s[d.platform] or {}
        s[d.platform].device = d
        M.save()
        V.message("Running on " .. d.name)
        if then_ then then_(d) end
      end)
    end)
  end
end

---- The *run* terminal

-- Runs the script in *run*, shown below without leaving this window.
local function run_script(project, script)
  local path = cache_dir() .. "/" .. slug(project.root) .. ".sh"
  if not write_file(path, script) then return V.message("Can't write " .. path) end
  local steps, count = nil, V.window_count()
  for i = 0, count - 1 do
    if V.buffer_name() == M.run_buffer then
      steps = i
      break
    end
    V.other_window(1)
  end
  if steps then
    V.terminal_run(M.run_buffer, "/bin/bash", { path }, project.root)
    if steps > 0 then V.other_window(-steps) end
  else
    V.split_window("below")
    local existed = false
    for _, name in ipairs(V.terminal_list()) do
      if name == M.run_buffer then existed = true end
    end
    V.terminal_run(M.run_buffer, "/bin/bash", { path }, project.root)
    if existed then V.switch_to_buffer(M.run_buffer) end
    V.select_window_in_direction("up")
  end
  core.state = "normal"
end

function M.log_path(root) return cache_dir() .. "/" .. slug(root) .. ".log" end
function M.state_path(root) return cache_dir() .. "/" .. slug(root) .. ".state" end

-- Shell pieces both scripts use.
local function prelude(project, dir)
  return table.concat({
    "#!/bin/bash",
    "set -o pipefail",
    "cd " .. q(dir) .. " || exit 1",
    "LOG=" .. q(M.log_path(project.root)),
    "STATE=" .. q(M.state_path(project.root)),
    ": > \"$LOG\"",
    "say() { printf '\\033[1m%s\\033[0m\\n' \"$*\"; }",
    "fail() { printf '\\033[1;31m✗ %s\\033[0m\\n' \"$*\"; exit 1; }",
    "",
  }, "\n")
end

---- iOS

function M.ios_script(project, container, scheme, device, build_only)
  local xcode = M.xcodebuild .. " " .. q("-" .. container.kind) .. " " .. q(container.path)
    .. " -scheme " .. q(scheme) .. " -configuration " .. q(M.configuration)
    .. " -destination " .. q("id=" .. device.udid)
  local lines = {
    prelude(project, project.root),
    "UDID=" .. q(device.udid),
    "say " .. q("▸ Building " .. scheme .. " for " .. device.name .. " (" .. device.os .. ")"),
    -- Booting while it builds saves the wait afterwards.
    build_only and "" or (M.xcrun .. " simctl boot \"$UDID\" >/dev/null 2>&1 &"),
    "if command -v xcbeautify >/dev/null 2>&1; then PRETTY=xcbeautify; else PRETTY=cat; fi",
    xcode .. " build 2>&1 | tee \"$LOG\" | $PRETTY || fail 'Build failed (SPC r e lists the errors)'",
  }
  if build_only then
    table.insert(lines, "say '✓ Built'")
    return table.concat(lines, "\n") .. "\n"
  end
  -- The .app: the first target in the scheme's build settings that makes one.
  for _, l in ipairs({
    "APP=$(" .. xcode .. " -showBuildSettings 2>/dev/null | awk '",
    "  /^Build settings for/ { if (app) exit; dir = \"\"; name = \"\"; app = 0 }",
    "  / TARGET_BUILD_DIR = / { sub(/.* = /, \"\"); dir = $0 }",
    "  / FULL_PRODUCT_NAME = / { sub(/.* = /, \"\"); name = $0 }",
    "  / WRAPPER_EXTENSION = app$/ { app = 1 }",
    "  END { if (app) print dir \"/\" name }')",
    "[ -d \"$APP\" ] || fail " .. q("No .app was built for " .. scheme),
    "BID=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' \"$APP/Info.plist\") || fail 'No bundle identifier'",
    "wait",
    M.xcrun .. " simctl boot \"$UDID\" >/dev/null 2>&1",
    (M.show_screen and M.headless_emulator) and ""
      or "open -a Simulator --args -CurrentDeviceUDID \"$UDID\" >/dev/null 2>&1",
    M.xcrun .. " simctl bootstatus \"$UDID\" >/dev/null 2>&1",
    "say \"▸ Installing $(basename \"$APP\")\"",
    M.xcrun .. " simctl install \"$UDID\" \"$APP\" || fail 'Install failed'",
    "echo \"ios $UDID $BID\" > \"$STATE\"",
    "say \"▸ Launched $BID; its output follows (SPC r k stops it)\"",
    "exec " .. M.xcrun .. " simctl launch --console-pty --terminate-running-process \"$UDID\" \"$BID\"",
  }) do
    table.insert(lines, l)
  end
  return table.concat(lines, "\n") .. "\n"
end

-- sourcekit-lsp reads an Xcode project through xcode-build-server's
-- buildServer.json, in the folder the language server starts in.
local function lsp_setup(project, container, scheme, quiet)
  if not V.on_path("xcode-build-server") then
    if not quiet then V.message("xcode-build-server isn't installed (brew install xcode-build-server)") end
    return
  end
  local args = { "config", "-" .. container.kind, container.path, "-scheme", scheme }
  spawn(project.root, "xcode-build-server", args, function(ok, out, err)
    if not ok then return V.message("xcode-build-server: " .. last_line(err ~= "" and err or out)) end
    V.lsp_restart()
    V.message("sourcekit-lsp set up for " .. scheme .. (quiet and "" or " (it knows the project after a build)"))
  end)
end

local function run_ios(project, device, build_only)
  with_scheme(project, false, function(container, scheme)
    if M.show_screen and not build_only then M.show_device(M.device_target(device)) end
    if not V.file_exists(project.root .. "/buildServer.json") then lsp_setup(project, container, scheme, true) end
    run_script(project, M.ios_script(project, container, scheme, device, build_only))
  end)
end

---- Android

function M.android_script(project, app, device, build_only)
  local variant = M.android_variant
  local lower = variant:sub(1, 1):lower() .. variant:sub(2)
  local lines = {
    prelude(project, app.dir),
    "ADB=" .. q(M.adb()),
    "EMULATOR=" .. q(M.emulator()),
    "SDK=" .. q(sdk()),
    "PKG=" .. q(app.package or ""),
    -- Gradle needs a JDK it supports: as Android Studio does, use the one
    -- it brings (a newer java on PATH may be too new), unless JAVA_HOME says.
    "if [ -z \"$JAVA_HOME\" ]; then",
    "  for j in '/Applications/Android Studio.app/Contents/jbr/Contents/Home' \"$HOME/Applications/Android Studio.app/Contents/jbr/Contents/Home\"; do",
    "    [ -d \"$j\" ] && export JAVA_HOME=\"$j\" && break",
    "  done",
    "fi",
  }
  if not build_only then
    if device.avd then
      for _, l in ipairs({
        "AVD=" .. q(device.avd),
        "find_avd() {",
        "  for s in $(\"$ADB\" devices | awk 'NR > 1 && $1 ~ /^emulator-/ { print $1 }'); do",
        "    [ \"$(\"$ADB\" -s \"$s\" emu avd name 2>/dev/null | head -1 | tr -d '\\r')\" = \"$AVD\" ] && { echo \"$s\"; return; }",
        "  done",
        "}",
        "SERIAL=$(find_avd)",
        "if [ -z \"$SERIAL\" ]; then",
        "  say \"▸ Starting the emulator $AVD\"",
        "  nohup \"$EMULATOR\" -avd \"$AVD\" " .. ((M.show_screen and M.headless_emulator) and "-no-window " or "")
          .. "> \"$LOG.emulator\" 2>&1 &",
        "  EMULATOR_PID=$!",
        "fi",
      }) do
        table.insert(lines, l)
      end
    else
      table.insert(lines, "SERIAL=" .. q(device.serial))
    end
  end
  table.insert(lines, "say " .. q("▸ Building " .. app.module .. " (" .. variant .. ")"))
  table.insert(lines, "./gradlew --console=plain " .. q(app.module .. ":assemble" .. variant)
    .. " 2>&1 | tee \"$LOG\" || fail 'Build failed (SPC r e lists the errors)'")
  -- The package the APK really has (applicationIdSuffix and flavors change it).
  for _, l in ipairs({
    "APK=$(ls -t " .. q(app.module_dir .. "/build/outputs/apk") .. "/*/*.apk " .. q(app.module_dir .. "/build/outputs/apk") .. "/*.apk 2>/dev/null | grep -i " .. q(lower) .. " | head -1)",
    "AAPT2=$(ls -d \"$SDK\"/build-tools/*/aapt2 2>/dev/null | sort -V | tail -1)",
    "if [ -n \"$APK\" ] && [ -n \"$AAPT2\" ]; then P=$(\"$AAPT2\" dump packagename \"$APK\" 2>/dev/null) && [ -n \"$P\" ] && PKG=$P; fi",
  }) do
    table.insert(lines, l)
  end
  if build_only then
    table.insert(lines, "say \"✓ Built ${APK:-the app}\"")
    return table.concat(lines, "\n") .. "\n"
  end
  if device.avd then
    for _, l in ipairs({
      "for i in $(seq 1 180); do",
      "  [ -n \"$SERIAL\" ] && break",
      -- One that quits at once says why in its output.
      "  if [ -n \"$EMULATOR_PID\" ] && ! kill -0 \"$EMULATOR_PID\" 2>/dev/null; then",
      "    grep -E 'ERROR|FATAL' \"$LOG.emulator\" | tail -3",
      "    fail \"The emulator $AVD quit (try it in Android Studio's Device Manager)\"",
      "  fi",
      "  sleep 1; SERIAL=$(find_avd)",
      "done",
      "[ -n \"$SERIAL\" ] || fail \"The emulator $AVD didn't start\"",
    }) do
      table.insert(lines, l)
    end
  end
  for _, l in ipairs({
    "export ANDROID_SERIAL=\"$SERIAL\"",
    "\"$ADB\" wait-for-device",
    "until [ \"$(\"$ADB\" shell getprop sys.boot_completed 2>/dev/null | tr -d '\\r')\" = 1 ]; do sleep 1; done",
    "say \"▸ Installing on $SERIAL\"",
    "./gradlew --console=plain -q " .. q(app.module .. ":install" .. variant) .. " 2>&1 | tee -a \"$LOG\" || fail 'Install failed'",
    "[ -n \"$PKG\" ] || fail 'No applicationId found (set it in build.gradle)'",
    "echo \"android $SERIAL $PKG\" > \"$STATE\"",
    "\"$ADB\" shell am force-stop \"$PKG\"",
    "\"$ADB\" shell monkey -p \"$PKG\" -c android.intent.category.LAUNCHER 1 >/dev/null 2>&1 || fail \"Couldn't launch $PKG\"",
    "for i in $(seq 1 20); do PID=$(\"$ADB\" shell pidof -s \"$PKG\" 2>/dev/null | tr -d '\\r'); [ -n \"$PID\" ] && break; sleep 0.5; done",
    "say \"▸ Launched $PKG; its log follows (SPC r k stops it)\"",
    "if [ -n \"$PID\" ]; then exec \"$ADB\" logcat -v color --pid=\"$PID\"; else exec \"$ADB\" logcat -v color; fi",
  }) do
    table.insert(lines, l)
  end
  return table.concat(lines, "\n") .. "\n"
end

---- Commands

local function go(build_only)
  local project = M.project()
  if not project.ios and not project.android then
    return V.message("No Xcode project or Android app (gradlew) in " .. project.root)
  end
  local s = M.settings(project.root)
  local function start(device)
    if device.platform == "ios" then
      if not project.ios then return V.message("No Xcode project here; SPC r d picks an Android device") end
      run_ios(project, device, build_only)
    else
      if not project.android then return V.message("No Android app here; SPC r d picks a simulator") end
      if M.show_screen and not build_only then M.show_device(M.device_target(device)) end
      run_script(project, M.android_script(project, project.android, device, build_only))
    end
  end
  local device = s.platform and s[s.platform] and s[s.platform].device
  if device and project[device.platform] then return start(device) end
  choose_device(project, start)
end

V.command("mobile-run", function() go(false) end)
V.command("mobile-build", function() go(true) end)
V.command("mobile-device", function() choose_device(M.project()) end)

V.command("mobile-scheme", function()
  local project = M.project()
  if not project.ios then return V.message("No Xcode project in " .. project.root) end
  with_scheme(project, true, function(_, scheme) V.message("Scheme: " .. scheme) end)
end)

V.command("mobile-lsp-setup", function()
  local project = M.project()
  if not project.ios then return V.message("No Xcode project in " .. project.root) end
  with_scheme(project, false, function(container, scheme) lsp_setup(project, container, scheme, false) end)
end)

V.command("mobile-show", function()
  for _, name in ipairs(V.terminal_list()) do
    if name == M.run_buffer then
      V.split_window("below")
      V.switch_to_buffer(M.run_buffer)
      core.state = "normal"
      return
    end
  end
  V.message("Nothing has run yet (SPC r r)")
end)

V.command("mobile-stop", function()
  local project = M.project()
  local state = read_file(M.state_path(project.root)) or ""
  local platform, device, id = state:match("^(%S+) (%S+) (%S+)")
  if not platform then return V.message("No app is running") end
  local program, args
  if platform == "ios" then
    program, args = M.xcrun, { "simctl", "terminate", device, id }
  else
    program, args = M.adb(), { "-s", device, "shell", "am", "force-stop", id }
  end
  spawn(project.root, program, args, function(ok, out, err)
    V.message(ok and ("Stopped " .. id) or ("Couldn't stop " .. id .. ": " .. last_line(err ~= "" and err or out)))
  end)
end)

-- The last build's errors and warnings, "path:line:col  error: what" (the
-- path relative to root when it's inside), from Swift, clang and Kotlin's
-- compilers alike.
function M.problems(log, root)
  local items, seen = {}, {}
  local function add(kind, path, line, col, what)
    if root and path:sub(1, #root + 1) == root .. "/" then path = path:sub(#root + 2) end
    local item = string.format("%s:%s:%s  %s: %s", path, line, col or 1, kind, what)
    if not seen[item] then
      seen[item] = true
      table.insert(items, item)
    end
  end
  for text in (log or ""):gmatch("[^\n]+") do
    local path, line, col, kind, what = text:match("^(/[^:]+):(%d+):(%d+): (%a+): (.*)$")
    if path and (kind == "error" or kind == "warning") then
      add(kind, path, line, col, what)
    else
      -- Kotlin: "e: file:///a/B.kt:12:5 what" or "e: /a/B.kt: (12, 5): what".
      local k, p, l, c, w = text:match("^([ew]): file://(/[^:]+):(%d+):(%d+) (.*)$")
      if not k then k, p, l, c, w = text:match("^([ew]): (/[^:]+): %((%d+), (%d+)%): (.*)$") end
      if k then add(k == "e" and "error" or "warning", p, l, c, w) end
    end
  end
  -- Errors first.
  local errors, warnings = {}, {}
  for _, item in ipairs(items) do
    table.insert(item:find("  error: ", 1, true) and errors or warnings, item)
  end
  for _, w in ipairs(warnings) do table.insert(errors, w) end
  return errors
end

V.command("mobile-errors", function()
  local project = M.project()
  local items = M.problems(read_file(M.log_path(project.root)), project.root)
  if #items == 0 then return V.message("The last build had no errors or warnings") end
  core.pick("Build problems", items, project.root, function(item) V.goto_location(item) end)
end)

---- The device's screen

-- What device.rs streams from: a phone's serial, "avd:NAME" or "ios:UDID".
function M.device_target(device)
  if device.platform == "ios" then return "ios:" .. device.udid end
  return device.avd and ("avd:" .. device.avd) or device.serial
end

-- Shows the screen to the right of this window (or where it's shown
-- already), keeping this window selected.
function M.show_device(target)
  for i = 0, V.window_count() - 1 do
    if V.buffer_name() == M.device_buffer then
      V.device_show(target, M.device_buffer)
      if i > 0 then V.other_window(-i) end
      return
    end
    V.other_window(1)
  end
  V.split_window("right")
  V.device_show(target, M.device_buffer)
  V.select_window_in_direction("left")
end

V.command("mobile-screen", function()
  local project = M.project()
  local s = M.settings(project.root)
  local device = s.platform and s[s.platform] and s[s.platform].device
  if device then return M.show_device(M.device_target(device)) end
  spawn(project.root, M.adb(), { "devices" }, function(ok, out)
    local serial = ok and out:match("\n(%S+)%s+device%f[%s]")
    if not serial then return V.message("No Android device (SPC r d picks one)") end
    M.show_device(serial)
  end)
end)

V.command("mobile-emulator-stop", function()
  local project = M.project()
  local s = M.settings(project.root)
  local device = s.platform and s[s.platform] and s[s.platform].device
  local target = V.device_here() and V.device_target() or (device and M.device_target(device))
  if not target then return V.message("No emulator or simulator chosen") end
  local udid = target:match("^ios:(.*)$")
  if udid then
    spawn(project.root, M.xcrun, { "simctl", "shutdown", udid }, function(ok, out, err)
      V.message(ok and "Shut the simulator down" or ("simctl: " .. last_line(err ~= "" and err or out)))
    end)
  elseif V.device_shell(target, "reboot -p") then
    V.message("Shutting the emulator down")
  else
    V.message("It isn't running")
  end
end)

local function button(name)
  return function()
    local why = V.device_button(V.device_target(), name)
    if why ~= "" then V.message(why) end
  end
end

-- Keys typed into the device (i in *device*).
function core.device_type(key, text)
  local why = V.device_key(V.device_target(), key, text)
  if why ~= "" then V.message(why) end
end

V.command("device-type", function()
  core.state = "insert"
  V.message("Typing into the device (ESC stops)")
end)
V.command("device-back", button("back"))
V.command("device-home", button("home"))
V.command("device-recents", button("recents"))
V.command("device-power", button("power"))
V.command("device-volume-up", button("volume-up"))
V.command("device-volume-down", button("volume-down"))

for key, command in pairs({ i = "device-type", a = "device-type", b = "device-back", H = "device-home",
    o = "device-recents", P = "device-power", ["+"] = "device-volume-up", ["-"] = "device-volume-down" }) do
  core.keymaps.device[key] = command
end
core.keymaps.device_insert.ESC = "normal-state"

for key, command in pairs({
  ["SPC r r"] = "mobile-run", ["SPC r b"] = "mobile-build", ["SPC r d"] = "mobile-device",
  ["SPC r s"] = "mobile-scheme", ["SPC r e"] = "mobile-errors", ["SPC r k"] = "mobile-stop",
  ["SPC r o"] = "mobile-show", ["SPC r L"] = "mobile-lsp-setup",
  ["SPC r v"] = "mobile-screen", ["SPC r Q"] = "mobile-emulator-stop",
}) do
  core.keymaps.normal[key] = command
end
