#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![cfg_attr(windows, feature(windows_process_extensions_show_window))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

#[cfg(windows)]
use std::sync::atomic::{AtomicIsize, AtomicU32};
use std::time::{Duration, Instant};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const PORT: u16 = 3080;

/// Public `dsh web` argv. `--no-open` is the documented flag that disables
/// opening the default browser after the Web UI binds.
const DSH_WEB_ARGS: &[&str] = &["web", "--no-open"];

/// Harness prints `dsh web: http://127.0.0.1:3080/?token=...` once the Loader
/// tree has settled. That line is the public ready signal; GET `/` without the
/// token now returns 401, so HTTP probing alone no longer works.
fn parse_dsh_web_url(line: &str) -> Option<String> {
    let idx = line.find("dsh web:")?;
    let rest = line[idx + "dsh web:".len()..].trim();
    if rest.starts_with("opening the default browser") {
        return None;
    }
    let url = rest.split_whitespace().next()?;
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url.to_string())
    } else {
        None
    }
}

fn pid_is_under_root_with(pid: u32, root: u32, parent_of: impl Fn(u32) -> Option<u32>) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        if current == root {
            return true;
        }
        match parent_of(current) {
            Some(parent) if parent != 0 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

fn allow_overlay_z_raise(picker_showing: bool, foreground_is_ours: bool) -> bool {
    !picker_showing && foreground_is_ours
}

/// The class every `IFileOpenDialog` carries.
const DIALOG_CLASS: &str = "#32770";

/// Only ever a native dialog, never just "a window the harness owns".
///
/// `#32770` is the class every `IFileOpenDialog` carries — but plenty of
/// unrelated resident apps (Internet Download Manager, for one) also give
/// their main window that class, so a lift is additionally restricted to
/// windows that just appeared, which is what the WinEvent hook reports.
/// Matching on ownership alone used to promote any harness-tree window, and
/// matching a long-lived foreign `#32770` used to hand it the foreground and
/// pull the caret out of the Harness composer.
fn should_lift_foreign_window(
    class: &str,
    window_pid: u32,
    our_pid: u32,
    in_dsh_tree: bool,
    foreground_is_ours: bool,
    is_visible_toplevel: bool,
) -> bool {
    if !is_visible_toplevel || window_pid == our_pid || class != DIALOG_CLASS {
        return false;
    }
    in_dsh_tree || foreground_is_ours
}

/// How to keep a foreign folder picker above Tauri without aborting it.
///
/// Harness `IFileOpenDialog::Show(NULL)` is already blocking in a Node child.
/// Reparenting (`GWLP_HWNDPARENT`) or `EnableWindow(FALSE)` on the foreground
/// shell mid-Show cancels that modal loop (`HRESULT_CANCELLED` → pick returns
/// null → composer stays a "choose workspace" trigger).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ForeignDialogLiftPlan {
    assign_owner: bool,
    disable_main: bool,
    restack_topmost: bool,
    show_window: bool,
    set_foreground: bool,
}

fn foreign_dialog_lift_plan(
    already_tracked: bool,
    dialog_is_foreground: bool,
) -> ForeignDialogLiftPlan {
    let restack = !(already_tracked && dialog_is_foreground);
    ForeignDialogLiftPlan {
        assign_owner: false,
        disable_main: false,
        restack_topmost: restack,
        show_window: restack && !already_tracked,
        // Take the foreground once, as the dialog appears. A second grab would
        // yank focus back out of whatever the user has clicked since.
        set_foreground: !already_tracked,
    }
}

fn remember_dsh_root_pid(pid: Option<u32>) {
    #[cfg(windows)]
    DSH_ROOT_PID.store(pid.unwrap_or(0), Ordering::SeqCst);
    #[cfg(not(windows))]
    let _ = pid;
}

fn url() -> String {
    format!("http://127.0.0.1:{PORT}")
}

struct ServerState {
    pid: Mutex<Option<u32>>,
    launch_url: Mutex<String>,
    ready: AtomicBool,
}

struct ShellState {
    minimized: AtomicBool,
    withdrawn: AtomicBool,
}

const OWNED_OVERLAY_LABELS: [&str; 2] = ["dsh", "chrome-btn"];

fn persist_window_state_for(label: &str) -> bool {
    label == "main"
}

/// Tray commands. The toolbar entry is the escape hatch for a floating button
/// that failed to surface: without it the CLI log tab is unreachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayAction {
    ShowChrome,
    Quit,
    None,
}

const TRAY_SHOW_CHROME_ID: &str = "show-chrome";
const TRAY_QUIT_ID: &str = "quit";

fn tray_menu_action(id: &str) -> TrayAction {
    match id {
        TRAY_SHOW_CHROME_ID => TrayAction::ShowChrome,
        TRAY_QUIT_ID => TrayAction::Quit,
        _ => TrayAction::None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MinimizeTransition {
    None,
    Minimized,
    Restored,
}

fn minimize_transition(was_minimized: bool, is_minimized: bool) -> MinimizeTransition {
    match (was_minimized, is_minimized) {
        (false, true) => MinimizeTransition::Minimized,
        (true, false) => MinimizeTransition::Restored,
        _ => MinimizeTransition::None,
    }
}

fn should_show_owned_overlay(main_minimized: bool, main_visible: bool) -> bool {
    main_visible && !main_minimized
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
enum LaunchSource {
    Npx,
    Local,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    source: LaunchSource,
    local_path: String,
    #[serde(default = "default_zoom")]
    zoom: f64,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            source: LaunchSource::Local,
            local_path: default_local_path(),
            zoom: default_zoom(),
        }
    }
}

fn default_zoom() -> f64 {
    1.0
}

fn default_local_path() -> String {
    #[cfg(windows)]
    {
        r"H:\github\deepseek-harness".to_string()
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/github/deepseek-harness")
    }
}

#[derive(Clone, serde::Serialize)]
struct Status {
    running: bool,
    port: u16,
    url: String,
    source: LaunchSource,
}

fn status_of(app: &AppHandle, running: bool) -> Status {
    let launch_url = app
        .state::<ServerState>()
        .launch_url
        .lock()
        .ok()
        .map(|g| g.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(url);
    Status {
        running,
        port: PORT,
        url: launch_url,
        source: load_settings(app).source,
    }
}

fn emit_server_ready(app: &AppHandle, launch_url: &str) {
    let state = app.state::<ServerState>();
    {
        let mut guard = state.launch_url.lock().unwrap();
        *guard = launch_url.to_string();
    }
    if state.ready.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = app.emit("server:ready", launch_url);
}

fn reset_server_ready(app: &AppHandle) {
    let state = app.state::<ServerState>();
    state.ready.store(false, Ordering::SeqCst);
    {
        let mut guard = state.launch_url.lock().unwrap();
        *guard = url();
    }
}

#[derive(Clone, serde::Serialize)]
struct UpgradeResult {
    ok: bool,
    version: String,
    restarted: bool,
    message: String,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法解析应用数据目录：{e}"))?;
    Ok(dir.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> AppSettings {
    let Ok(path) = settings_path(app) else {
        return AppSettings::default();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return AppSettings::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_settings(app: &AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("无法创建设置目录：{e}"))?;
    }
    let text = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("无法序列化设置：{e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("无法写入设置：{e}"))
}

/// WebView2's own zoom range.
const ZOOM_MIN: f64 = 0.25;
const ZOOM_MAX: f64 = 5.0;
/// Below this the two zooms are the same rung of the browser's ladder, so a
/// reading that only differs by float noise does not rewrite the settings file.
const ZOOM_EPSILON: f64 = 0.005;

fn clamp_zoom(zoom: f64) -> f64 {
    if !zoom.is_finite() {
        return 1.0;
    }
    (zoom.clamp(ZOOM_MIN, ZOOM_MAX) * 100.0).round() / 100.0
}

/// Recover the zoom factor from the page's device pixel ratio, which is the
/// monitor scale multiplied by that zoom. WebView2 owns the zoom hotkeys and
/// Tauri exposes neither a getter nor a change event, so reading the ratio back
/// out of the page is the only way to learn what the user picked.
///
/// An unusable probe reads as 1.0 rather than an error, and the caller only
/// writes on a real change, so it cannot clobber the saved zoom either way.
fn observed_zoom(device_pixel_ratio: Option<f64>, scale_factor: f64) -> f64 {
    let Some(dpr) = device_pixel_ratio else {
        return 1.0;
    };
    if !dpr.is_finite() || dpr <= 0.0 || !scale_factor.is_finite() || scale_factor <= 0.0 {
        return 1.0;
    }
    clamp_zoom(dpr / scale_factor)
}

fn zoom_changed(saved: f64, observed: f64) -> bool {
    (saved - observed).abs() > ZOOM_EPSILON
}

/// Last device pixel ratio read from the page, as `f64::to_bits`. Zero means
/// nothing has been read yet.
static LAST_DPR: AtomicU64 = AtomicU64::new(0);

/// Whether a reading is a real move in the page's device pixel ratio, and so
/// evidence that the user changed the zoom.
///
/// The saved zoom is only rewritten on such a move. Should a runtime ever stop
/// folding its zoom factor into the ratio, the poll then leaves the setting
/// alone instead of resetting it the first time it runs after launch.
fn dpr_moved(previous: Option<f64>, current: f64) -> bool {
    previous.is_some_and(|previous| (previous - current).abs() > 1e-6)
}

fn record_dpr(current: f64) -> bool {
    let previous = match LAST_DPR.swap(current.to_bits(), Ordering::Relaxed) {
        0 => None,
        bits => Some(f64::from_bits(bits)),
    };
    dpr_moved(previous, current)
}

fn remember_dsh_zoom(app: &AppHandle, zoom: f64) {
    let mut settings = load_settings(app);
    if !zoom_changed(settings.zoom, zoom) {
        return;
    }
    settings.zoom = zoom;
    let _ = save_settings(app, &settings);
}

fn validate_local_repo(path: &str) -> Result<PathBuf, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("本地仓库路径不能为空".to_string());
    }
    let dir = PathBuf::from(trimmed);
    if !dir.is_dir() {
        return Err(format!("本地仓库不存在：{trimmed}"));
    }
    if !dir.join("package.json").is_file() || !dir.join("apps").join("cli").is_dir() {
        return Err(format!(
            "不是有效的 DeepSeek Harness 仓库（缺少 package.json 或 apps/cli）：{trimmed}"
        ));
    }
    Ok(dir)
}

fn path_sep() -> char {
    if cfg!(windows) { ';' } else { ':' }
}

/// Directories a GUI-launched process often lacks on PATH.
fn extra_tool_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Ok(base) = std::env::var(var) {
                let candidate = if var == "LOCALAPPDATA" {
                    PathBuf::from(format!("{base}\\Programs\\nodejs"))
                } else {
                    PathBuf::from(format!("{base}\\nodejs"))
                };
                if candidate.is_dir() {
                    dirs.push(candidate);
                }
            }
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            let npm = PathBuf::from(format!("{appdata}\\npm"));
            if npm.is_dir() {
                dirs.push(npm);
            }
            let nvm = PathBuf::from(format!("{appdata}\\nvm"));
            if nvm.is_dir() {
                dirs.push(nvm);
            }
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            for rel in [r"pnpm", r"Volta\bin", r"fnm"] {
                let p = PathBuf::from(format!("{local}\\{rel}"));
                if p.is_dir() {
                    dirs.push(p);
                }
            }
        }
        for git in [r"C:\Program Files\Git\cmd", r"C:\Program Files (x86)\Git\cmd"] {
            let p = PathBuf::from(git);
            if p.is_dir() {
                dirs.push(p);
            }
        }
    }
    #[cfg(not(windows))]
    {
        for p in [
            "/opt/homebrew/bin",
            "/opt/homebrew/opt/fnm/bin",
            "/usr/local/bin",
        ] {
            let path = PathBuf::from(p);
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    dirs
}

fn apply_child_path(c: &mut Command, extra: &[PathBuf]) {
    let mut dirs: Vec<String> = extra
        .iter()
        .filter(|p| p.is_dir())
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    for p in extra_tool_dirs() {
        let s = p.to_string_lossy().into_owned();
        if !dirs.iter().any(|d| d == &s) {
            dirs.push(s);
        }
    }
    if dirs.is_empty() {
        return;
    }
    let sep = path_sep();
    let mut path = dirs.join(&sep.to_string());
    if let Ok(p) = std::env::var("PATH") {
        if !p.is_empty() {
            path.push(sep);
            path.push_str(&p);
        }
    }
    c.env("PATH", path);
}

fn find_in_dirs(name: &str, extra: &[PathBuf]) -> Option<PathBuf> {
    let mut dirs = extra.to_vec();
    dirs.extend(extra_tool_dirs());
    if let Ok(path) = std::env::var("PATH") {
        for part in path.split(path_sep()) {
            if !part.is_empty() {
                dirs.push(PathBuf::from(part));
            }
        }
    }
    let candidates = if cfg!(windows) {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    };
    for dir in dirs {
        for file in &candidates {
            let p = dir.join(file);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn find_node() -> Result<PathBuf, String> {
    find_in_dirs("node", &[])
        .filter(|p| match p.extension().and_then(|e| e.to_str()) {
            Some(ext) => !ext.eq_ignore_ascii_case("cmd"),
            None => true,
        })
        .ok_or_else(|| {
            "找不到 node。从资源管理器启动时 PATH 往往没有 Node.js，请安装 Node.js 22+".to_string()
        })
}

fn local_source_bin(dir: &Path) -> PathBuf {
    dir.join("apps").join("cli").join("src").join("bin.ts")
}

/// Official source vector: `node --import tsx/esm apps/cli/src/bin.ts`.
/// Does not go through `pnpm`, so it still works when this exe is launched
/// from `target/release` (or anywhere else) rather than a repo root.
fn local_dsh_command(dir: &Path, extra_args: &[&str]) -> Result<Command, String> {
    let node = find_node()?;
    let bin = local_source_bin(dir);
    if !bin.is_file() {
        return Err(format!("找不到源码入口：{}", bin.display()));
    }
    let mut extras = Vec::new();
    if let Some(parent) = node.parent() {
        extras.push(parent.to_path_buf());
    }
    extras.push(dir.join("node_modules").join(".bin"));
    let mut c = Command::new(&node);
    c.arg("--import").arg("tsx/esm").arg(&bin);
    c.args(extra_args);
    c.current_dir(dir);
    apply_child_path(&mut c, &extras);
    #[cfg(windows)]
    hide_console(&mut c);
    Ok(c)
}

/// Base tool launcher for non-Windows (macOS/Linux). Prefers an explicit
/// fnm path so the app also works when launched from Finder/Dock, where the
/// GUI process has a minimal PATH without node/npx/pnpm; falls back to PATH.
#[cfg(not(windows))]
fn base_tool_cmd(tool: &str) -> Command {
    for fnm in [
        "/opt/homebrew/bin/fnm",
        "/opt/homebrew/opt/fnm/bin/fnm",
        "/usr/local/bin/fnm",
    ] {
        if Path::new(fnm).is_file() {
            let mut c = Command::new(fnm);
            c.args(["exec", "--using", "default", "--", tool]);
            return c;
        }
    }
    let mut c = Command::new(tool);
    apply_child_path(&mut c, &[]);
    c
}

/// Windows: keep helper consoles hidden (git / pnpm / taskkill).
#[cfg(windows)]
fn hide_console(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

/// Hidden console that can still receive Ctrl+C.
///
/// `CREATE_NO_WINDOW` leaves Node with no console, so the only stop left is
/// `taskkill /T /F`. That tears down IDA MCP in-process and can corrupt `*.i64`.
/// `CREATE_NEW_CONSOLE` + `SW_HIDE` keeps a console we can AttachConsole to.
#[cfg(windows)]
fn hide_dsh_console(c: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    const SW_HIDE: u16 = 0;
    c.creation_flags(CREATE_NEW_CONSOLE);
    c.show_window(SW_HIDE);
}

/// Windows: GUI-launched processes may inherit a stale PATH (Node.js not
/// visible), and Rust's Command::new("npx") cannot resolve the .cmd batch
/// shim the way cmd.exe does. So run through "cmd /C <tool> ..." — exactly
/// like a user typing it in a terminal — and merge standard Node/pnpm/git
/// install directories into PATH as a safety net.
#[cfg(windows)]
fn base_tool_cmd(tool: &str) -> Command {
    let mut c = Command::new("cmd");
    c.args(["/C", tool]);
    apply_child_path(&mut c, &[]);
    hide_console(&mut c);
    c
}

fn git_cmd() -> Command {
    #[cfg(windows)]
    {
        base_tool_cmd("git")
    }
    #[cfg(not(windows))]
    {
        Command::new("git")
    }
}

fn dsh_command(settings: &AppSettings) -> Result<Command, String> {
    match settings.source {
        LaunchSource::Npx => {
            let mut c = base_tool_cmd("npx");
            c.args(["--yes", "@deepseek-ai/dsh"]);
            c.args(DSH_WEB_ARGS);
            #[cfg(windows)]
            hide_dsh_console(&mut c);
            Ok(c)
        }
        LaunchSource::Local => {
            let dir = validate_local_repo(&settings.local_path)?;
            let mut c = local_dsh_command(&dir, DSH_WEB_ARGS)?;
            #[cfg(windows)]
            hide_dsh_console(&mut c);
            Ok(c)
        }
    }
}

fn version_command(settings: &AppSettings) -> Result<Command, String> {
    match settings.source {
        LaunchSource::Npx => {
            // NOTE: no "--" separator — npx passes everything after the package
            // name to the binary, and dsh would receive "-- --version".
            let mut c = base_tool_cmd("npx");
            c.args(["--yes", "@deepseek-ai/dsh", "--version"]);
            c.env("npm_config_update_notifier", "false");
            Ok(c)
        }
        LaunchSource::Local => {
            let dir = validate_local_repo(&settings.local_path)?;
            local_dsh_command(&dir, &["--version"])
        }
    }
}

fn npx_upgrade_command() -> Command {
    // @latest tag makes npx fetch the newest release (the --latest flag is
    // not a valid npx option and only produces npm warnings).
    let mut c = base_tool_cmd("npx");
    c.args(["--yes", "@deepseek-ai/dsh@latest", "--version"]);
    c.env("npm_config_update_notifier", "false");
    c
}

fn local_upgrade_steps(dir: &Path) -> [(&'static str, Command); 3] {
    let mut pull = git_cmd();
    pull.args(["pull"]);
    pull.current_dir(dir);

    let mut install = base_tool_cmd("pnpm");
    install.args(["install"]);
    install.current_dir(dir);

    let mut build = base_tool_cmd("pnpm");
    build.args(["run", "build"]);
    build.current_dir(dir);

    [("git pull", pull), ("pnpm install", install), ("pnpm run build", build)]
}

fn extract_version(lines: &[String]) -> Option<String> {
    // Scan from the last line backwards for the first semver pattern, so it
    // works with bare "0.1.0", "v0.1.0", "dsh 0.1.0-rc.6", npm download lines
    // ("...dsh-0.1.0.tgz"), etc.
    for line in lines.iter().rev() {
        if let Some(v) = find_semver(line) {
            return Some(v);
        }
    }
    None
}

/// Find a semver-like pattern (digits.digits.digits with optional
/// -prerelease / +build suffix) anywhere inside a line.
fn find_semver(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i;
        let mut dots = 0u32;
        while j < n {
            if b[j].is_ascii_digit() {
                j += 1;
            } else if b[j] == b'.' && dots < 2 && j + 1 < n && b[j + 1].is_ascii_digit() {
                dots += 1;
                j += 1;
            } else {
                break;
            }
        }
        if dots == 2 {
            let mut end = j;
            if end < n && b[end] == b'-' {
                let mut k = end + 1;
                while k < n
                    && (b[k].is_ascii_alphanumeric() || b[k] == b'.' || b[k] == b'-')
                {
                    k += 1;
                }
                end = k;
            }
            if end > start {
                return Some(s[start..end].to_string());
            }
        }
        i = if j > i { j } else { i + 1 };
    }
    None
}

fn is_port_open(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// dsh web binds the port immediately, then answers 404 until frontend-static
/// mounts. The real page injects `window.__DSH_BOOT__`; TCP-only readiness
/// loads that 404 into the iframe and leaves a white screen.
fn looks_like_dsh_index(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    let status_ok = text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200");
    status_ok && text.contains("__DSH_BOOT__")
}

fn looks_like_unauthorized(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    text.starts_with("HTTP/1.1 401") || text.starts_with("HTTP/1.0 401")
}

/// `/api/events.host` returns 426 once the connection plugin has mounted.
fn looks_like_dsh_api(buf: &[u8]) -> bool {
    let text = String::from_utf8_lossy(buf);
    text.starts_with("HTTP/1.1 426") || text.starts_with("HTTP/1.0 426")
}

fn http_get(port: u16, path: &str) -> Option<Vec<u8>> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Some(buf)
}

fn is_web_ready(port: u16) -> bool {
    let index = http_get(port, "/").unwrap_or_default();
    if !looks_like_dsh_index(&index) {
        return false;
    }
    let api = http_get(port, "/api/events.host").unwrap_or_default();
    looks_like_dsh_api(&api)
}

fn spawn_readiness_poller(app: AppHandle, require_owned: bool) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if app.state::<ServerState>().ready.load(Ordering::SeqCst) {
                return;
            }
            if is_web_ready(PORT) {
                emit_server_ready(&app, &url());
                return;
            }
            if require_owned && app.state::<ServerState>().pid.lock().unwrap().is_none() {
                return;
            }
            if Instant::now() >= deadline {
                if !app.state::<ServerState>().ready.load(Ordering::SeqCst) {
                    let _ = app.emit("server:timeout", ());
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

/// Harness `PROCESS_SHUTDOWN_TIMEOUT_MS` is 5s. Wait longer so plugin dispose
/// (IDA MCP flushing `*.i64`) can finish before we escalate to a tree kill.
const DSH_STOP_GRACE: Duration = Duration::from_secs(8);

fn dsh_tree_has_stopped(root_alive: bool, port_open: bool) -> bool {
    !root_alive && !port_open
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
    #[cfg(windows)]
    unsafe {
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        let handle = win32::OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return false;
        }
        let mut code = 0u32;
        let ok = win32::GetExitCodeProcess(handle, &mut code);
        win32::CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

fn wait_for_dsh_stop(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if dsh_tree_has_stopped(process_is_alive(pid), is_port_open(PORT)) {
            return true;
        }
        if Instant::now() >= deadline {
            return dsh_tree_has_stopped(process_is_alive(pid), is_port_open(PORT));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(windows)]
fn send_ctrl_c(pid: u32) -> bool {
    unsafe {
        // Debug `cargo run` attaches a console; AttachConsole fails until we drop it.
        let _ = win32::FreeConsole();
        if win32::AttachConsole(pid) == 0 {
            return false;
        }
        // Ignore the Ctrl+C we are about to generate, or this process exits too.
        win32::SetConsoleCtrlHandler(None, 1);
        let sent = win32::GenerateConsoleCtrlEvent(0, 0) != 0;
        std::thread::sleep(Duration::from_millis(150));
        let _ = win32::FreeConsole();
        win32::SetConsoleCtrlHandler(None, 0);
        sent
    }
}

fn request_graceful_stop(pid: u32) {
    #[cfg(unix)]
    unsafe {
        // Signal the supervisor only. `kill(-pid, SIGTERM)` would also hit
        // MCP children (IDA) at the same moment and skip harness dispose.
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = send_ctrl_c(pid);
    }
}

fn force_kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"]);
        hide_console(&mut c);
        let _ = c.status();
    }
}

fn kill_process_group(pid: u32) {
    // One SIGTERM/Ctrl+C only, then wait. A second signal makes harness
    // force-exit and skip plugin dispose (the path that closes IDA cleanly).
    // Unix and Windows share DSH_STOP_GRACE; do not SIGKILL after a short sleep.
    request_graceful_stop(pid);
    if wait_for_dsh_stop(pid, DSH_STOP_GRACE) {
        return;
    }
    force_kill_tree(pid);
    let _ = wait_for_dsh_stop(pid, Duration::from_secs(2));
}

fn pipe_lines<R: std::io::Read + Send + 'static>(
    reader: R,
    app: AppHandle,
    event: &'static str,
    collected: Option<std::sync::Arc<Mutex<Vec<String>>>>,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(l) => {
                    let _ = app.emit(event, &l);
                    if event == "server:stdout" {
                        if let Some(launch_url) = parse_dsh_web_url(&l) {
                            emit_server_ready(&app, &launch_url);
                        }
                    }
                    if let Some(buf) = &collected {
                        buf.lock().unwrap().push(l);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn start_internal(app: &AppHandle) -> Result<Status, String> {
    {
        let state = app.state::<ServerState>();
        if state.pid.lock().unwrap().is_some() {
            return Ok(status_of(app, true));
        }
    }

    reset_server_ready(app);

    // Port already serving a fully-booted dsh that still allows unauthenticated
    // index reads (pre-token builds). Token-gated servers print `dsh web:` with
    // `?token=` instead; HTTP GET `/` is 401 and cannot be reused.
    if is_web_ready(PORT) {
        emit_server_ready(app, &url());
        return Ok(status_of(app, true));
    }
    if is_port_open(PORT) {
        let index = http_get(PORT, "/").unwrap_or_default();
        if looks_like_unauthorized(&index) {
            return Err(
                "端口 3080 已被需要启动令牌的 dsh 占用，请先结束该 Node 进程后再启动".to_string(),
            );
        }
        spawn_readiness_poller(app.clone(), false);
        return Ok(status_of(app, true));
    }

    let settings = load_settings(app);
    let mut cmd = dsh_command(&settings)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let spawn_err = match settings.source {
        LaunchSource::Npx => "无法启动 npx @deepseek-ai/dsh web".to_string(),
        LaunchSource::Local => format!(
            "无法启动本地 dsh（node --import tsx/esm apps/cli/src/bin.ts web @ {}）",
            settings.local_path.trim()
        ),
    };
    let mut child = cmd.spawn().map_err(|e| format!("{spawn_err}：{e}"))?;
    let pid = child.id();

    if let Some(stdout) = child.stdout.take() {
        pipe_lines(stdout, app.clone(), "server:stdout", None);
    }
    if let Some(stderr) = child.stderr.take() {
        pipe_lines(stderr, app.clone(), "server:stderr", None);
    }

    {
        let state = app.state::<ServerState>();
        let mut guard = state.pid.lock().unwrap();
        *guard = Some(pid);
    }
    remember_dsh_root_pid(Some(pid));
    #[cfg(windows)]
    unsafe {
        win32::AllowSetForegroundWindow(pid);
    }

    // watcher: clear state and notify on exit
    {
        let app = app.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            let state = app.state::<ServerState>();
            let mut guard = state.pid.lock().unwrap();
            *guard = None;
            drop(guard);
            remember_dsh_root_pid(None);
            reset_server_ready(&app);
            let _ = app.emit("server:exited", code);
        });
    }

    spawn_readiness_poller(app.clone(), true);

    Ok(status_of(app, true))
}

#[tauri::command]
fn get_settings(app: AppHandle) -> AppSettings {
    load_settings(&app)
}

#[tauri::command]
fn set_settings(app: AppHandle, mut settings: AppSettings) -> Result<AppSettings, String> {
    if settings.source == LaunchSource::Local {
        validate_local_repo(&settings.local_path)?;
    }
    // The zoom is observed from the page rather than edited in the settings UI,
    // whose payload carries no zoom and would reset it to the serde default.
    settings.zoom = load_settings(&app).zoom;
    save_settings(&app, &settings)?;
    Ok(settings)
}

#[tauri::command]
fn start_server(app: AppHandle) -> Result<Status, String> {
    start_internal(&app)
}

#[tauri::command]
fn stop_server(app: AppHandle) -> Status {
    let state = app.state::<ServerState>();
    let pid = {
        let mut guard = state.pid.lock().unwrap();
        guard.take()
    };
    remember_dsh_root_pid(None);
    reset_server_ready(&app);
    match pid {
        Some(pid) => {
            kill_process_group(pid);
            let _ = app.emit("server:stopped", ());
            status_of(&app, false)
        }
        // Nothing we own: reflect whether 3080 is still up (reused server).
        None => status_of(&app, is_port_open(PORT)),
    }
}

#[tauri::command]
fn restart_server(app: AppHandle) -> Result<Status, String> {
    stop_server(app.clone());
    std::thread::sleep(Duration::from_millis(600));
    start_internal(&app)
}

fn run_logged_command(
    app: &AppHandle,
    mut cmd: Command,
    collected: &std::sync::Arc<Mutex<Vec<String>>>,
) -> Result<bool, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("无法启动命令：{e}"))?;
    if let Some(stdout) = child.stdout.take() {
        pipe_lines(
            stdout,
            app.clone(),
            "upgrade:stdout",
            Some(collected.clone()),
        );
    }
    if let Some(stderr) = child.stderr.take() {
        pipe_lines(
            stderr,
            app.clone(),
            "upgrade:stderr",
            Some(collected.clone()),
        );
    }
    let exit = child.wait();
    Ok(matches!(exit, Ok(s) if s.success()))
}

fn restart_after_upgrade(app: &AppHandle) -> (bool, String) {
    let owns = app.state::<ServerState>().pid.lock().unwrap().is_some();
    if owns {
        stop_server(app.clone());
        std::thread::sleep(Duration::from_millis(600));
        match start_internal(app) {
            Ok(_) => (true, "升级完成，服务已用新版本重启".to_string()),
            Err(e) => (false, format!("升级成功，但重启失败：{e}，请手动点击重启")),
        }
    } else {
        (
            false,
            "升级完成，已安装最新版（当前无本应用运行的服务，下次启动即生效）".to_string(),
        )
    }
}

fn upgrade_dsh_inner(app: AppHandle) -> Result<UpgradeResult, String> {
    let settings = load_settings(&app);
    let collected: std::sync::Arc<Mutex<Vec<String>>> =
        std::sync::Arc::new(Mutex::new(Vec::new()));

    let ok = match settings.source {
        LaunchSource::Npx => {
            let cmd = npx_upgrade_command();
            run_logged_command(&app, cmd, &collected).map_err(|e| {
                format!("无法启动升级命令（npx --yes @deepseek-ai/dsh@latest --version）：{e}")
            })?
        }
        LaunchSource::Local => {
            let dir = validate_local_repo(&settings.local_path)?;
            let mut all_ok = true;
            for (label, cmd) in local_upgrade_steps(&dir) {
                let _ = app.emit("upgrade:stdout", format!("$ {label}"));
                collected.lock().unwrap().push(format!("$ {label}"));
                match run_logged_command(&app, cmd, &collected) {
                    Ok(true) => {}
                    Ok(false) => {
                        all_ok = false;
                        break;
                    }
                    Err(e) => return Err(format!("无法启动 {label}：{e}")),
                }
            }
            all_ok
        }
    };

    std::thread::sleep(Duration::from_millis(150));
    let lines = collected.lock().unwrap().clone();
    let version = extract_version(&lines).unwrap_or_else(|| "unknown".to_string());

    let (restarted, message) = if ok {
        restart_after_upgrade(&app)
    } else {
        let fail = match settings.source {
            LaunchSource::Npx => "升级失败，请检查网络后重试".to_string(),
            LaunchSource::Local => {
                "升级失败（git pull / pnpm install / pnpm run build），请查看 CLI 日志".to_string()
            }
        };
        (false, fail)
    };

    Ok(UpgradeResult {
        ok,
        version,
        restarted,
        message,
    })
}

#[tauri::command]
async fn upgrade_dsh(app: AppHandle) -> Result<UpgradeResult, String> {
    tauri::async_runtime::spawn_blocking(move || upgrade_dsh_inner(app))
        .await
        .map_err(|e| format!("升级任务失败：{e}"))?
}

/// Report the DeepSeek Harness (dsh) CLI version that will actually run.
#[tauri::command]
async fn dsh_version(app: AppHandle) -> Result<String, String> {
    let settings = load_settings(&app);
    let mut cmd = version_command(&settings)?;
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("无法获取 dsh 版本：{e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("读取 dsh 版本失败：{e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let version = extract_version(&lines).unwrap_or_else(|| "unknown".to_string());
    Ok(version)
}

#[tauri::command]
fn server_status(app: AppHandle) -> Status {
    let state = app.state::<ServerState>();
    let running = state.pid.lock().unwrap().is_some() || is_port_open(PORT);
    status_of(&app, running)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct DshAvoidRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct DshTheme {
    dark: bool,
    bg: String,
    fg: String,
    border: String,
    #[serde(default)]
    avoid: Option<DshAvoidRect>,
    /// `window.devicePixelRatio`, read here because the theme poll is already
    /// the one thing reaching into the page on a timer. Only the host reads it,
    /// to recover the zoom factor, so it is not passed on to the frontend.
    #[serde(default, skip_serializing)]
    dpr: Option<f64>,
    /// Whether our own CSS is still on the page. False on the first poll and
    /// after a reload, which is the only time it is worth injecting again.
    #[serde(default, skip_serializing)]
    styled: bool,
}

/// Header export capsule labels. Harness zh is `Session 日志`, not `Session log`.
/// Kept next to `READ_DSH_THEME_JS` so the probe and tests share one list.
#[cfg(test)]
const SESSION_EXPORT_LABELS: &[&str] = &["Session log", "Session 日志"];

#[cfg(test)]
fn session_export_anchor_matches(text: &str) -> bool {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    SESSION_EXPORT_LABELS
        .iter()
        .any(|label| normalized == *label || normalized.starts_with(label))
}

const READ_DSH_THEME_JS: &str = r#"
(() => {
  try {
    const body = document.body;
    if (!body) return null;
    const cs = getComputedStyle(body);
    const token = (name) => (cs.getPropertyValue(name) || "").trim();
    const bg = token("--dsw-alias-button-floating-fill")
      || token("--dsw-alias-bg-layer-2")
      || token("--dsw-specific-sidebar-fill")
      || cs.backgroundColor;
    const fg = token("--dsw-alias-label-primary") || cs.color;
    const border = token("--dsw-alias-border-l2")
      || token("--dsw-alias-border-l1")
      || "transparent";
    const dark = body.hasAttribute("data-ds-dark-theme")
      || document.documentElement.style.colorScheme === "dark";
    const labels = ["Session log", "Session 日志"];
    const norm = (el) => (el.textContent || "").replace(/\s+/g, " ").trim();
    const match = (el) => labels.some((l) => { const t = norm(el); return t === l || t.startsWith(l); });
    // Cached across polls: scanning every button in a long conversation is by
    // far the costliest thing here, and the capsule outlives the scan.
    let sessionLog = window.__dshDesktopAnchor;
    if (!sessionLog || !sessionLog.isConnected || !match(sessionLog)) {
      sessionLog = Array.from(document.querySelectorAll("header button")).find(match)
        || Array.from(document.querySelectorAll("button")).find(match)
        || null;
      window.__dshDesktopAnchor = sessionLog;
    }
    let avoid = null;
    if (sessionLog) {
      const r = sessionLog.getBoundingClientRect();
      if (r.width > 0 && r.height > 0) {
        avoid = { x: r.left, y: r.top, w: r.width, h: r.height };
      }
    }
    const styled = document.documentElement.classList.contains("dsh-desktop-wide-chat");
    return { dark, bg, fg, border, avoid, dpr: window.devicePixelRatio, styled };
  } catch (e) {
    return null;
  }
})()
"#;

/// Read live theme tokens from the Harness page and broadcast them.
#[tauri::command]
fn sync_dsh_theme(app: AppHandle) {
    let Some(wdw) = app.get_webview_window("dsh") else {
        return;
    };
    let app = app.clone();
    let scale_factor = wdw.scale_factor().unwrap_or(1.0);
    let _ = wdw.eval_with_callback(READ_DSH_THEME_JS, move |json| {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else {
            return;
        };
        if value.is_null() {
            return;
        }
        let Ok(mut theme) = serde_json::from_value::<DshTheme>(value) else {
            return;
        };
        if theme.bg.is_empty() {
            return;
        }
        if !theme.styled {
            if let Some(wdw) = app.get_webview_window("dsh") {
                inject_dsh_desktop_css(&wdw);
            }
        }
        let zoom = observed_zoom(theme.dpr, scale_factor);
        if let Some(dpr) = theme.dpr.filter(|d| d.is_finite() && *d > 0.0) {
            if record_dpr(dpr) {
                remember_dsh_zoom(&app, zoom);
            }
        }
        // The probe measures in CSS pixels; overlays are placed in logical
        // pixels, and the zoom factor is the ratio between the two.
        if let Some(avoid) = theme.avoid.as_mut() {
            avoid.x *= zoom;
            avoid.y *= zoom;
            avoid.w *= zoom;
            avoid.h *= zoom;
        }
        let _ = app.emit("dsh:theme", theme);
    });
}

const INJECT_WIDE_CHAT_JS: &str = r#"
(() => {
  const head = document.head || document.documentElement;
  const wideId = "dsh-desktop-wide-chat";
  if (!document.getElementById(wideId)) {
    const style = document.createElement("style");
    style.id = wideId;
    style.textContent = "html.dsh-desktop-wide-chat,html.dsh-desktop-wide-chat *{--dsh-chat-content-width:100%!important;--dsh-composer-card-max-width:100%!important;}";
    document.documentElement.classList.add("dsh-desktop-wide-chat");
    head.appendChild(style);
  }

  const fontId = "dsh-desktop-lxgw-font";
  if (!document.getElementById(fontId)) {
    const names = [
      "霞鹜文楷等宽 GB 屏幕阅读版",
      "LXGW WenKai Mono GB Screen",
    ];
    const pick = (candidates) => {
      const ctx = document.createElement("canvas").getContext("2d");
      if (!ctx) return null;
      const sample = "mmmmmmmmlli汉字國國";
      for (const name of candidates) {
        for (const base of ["monospace", "serif", "sans-serif"]) {
          ctx.font = "72px " + base;
          const fallback = ctx.measureText(sample).width;
          ctx.font = '72px "' + name + '", ' + base;
          if (ctx.measureText(sample).width !== fallback) return name;
        }
      }
      return null;
    };
    const name = pick(names);
    if (name) {
      const style = document.createElement("style");
      style.id = fontId;
      const quoted = '"' + name + '"';
      style.textContent =
        "html.dsh-desktop-lxgw,html.dsh-desktop-lxgw body,html.dsh-desktop-lxgw *:not(svg):not(path){font-family:" +
        quoted +
        ",monospace!important;}" +
        "html.dsh-desktop-lxgw{--dsw-font-family:" +
        quoted +
        ",monospace!important;}";
      document.documentElement.classList.add("dsh-desktop-lxgw");
      head.appendChild(style);
    }
  }
  return true;
})()
"#;

/// Widen the Conversation tab past Harness's 748px content cap.
///
/// Driven by the theme poll's `styled` flag rather than run on every tick: the
/// script is idempotent, but its font probe measures text on a canvas, and on
/// a machine without the font installed that measuring would never stop.
///
/// Ctrl +/-/0 zoom is deliberately not bound here: `zoomHotkeysEnabled` leaves
/// WebView2's `IsZoomControlEnabled` and browser accelerator keys on, so the
/// runtime already handles those keys whenever this webview holds focus.
fn inject_dsh_desktop_css(wdw: &tauri::WebviewWindow) {
    let _ = wdw.eval(INJECT_WIDE_CHAT_JS);
}

/// Re-apply the zoom the user last left the page at, since a fresh WebView2
/// instance always starts at 1.0.
///
/// A host-set zoom becomes the webview's new default: it survives navigation,
/// and Ctrl+0 returns to it rather than to 100%.
#[tauri::command]
fn restore_dsh_zoom(app: AppHandle) {
    let Some(wdw) = app.get_webview_window("dsh") else {
        return;
    };
    let _ = wdw.set_zoom(clamp_zoom(load_settings(&app).zoom));
}

#[cfg(windows)]
static DSH_ROOT_PID: AtomicU32 = AtomicU32::new(0);
#[cfg(windows)]
static PICKER_HWND: AtomicIsize = AtomicIsize::new(0);

#[cfg(windows)]
mod win32 {
    #[repr(C)]
    pub struct ProcessEntry32W {
        pub dw_size: u32,
        pub cnt_usage: u32,
        pub th32_process_id: u32,
        pub th32_default_heap_id: usize,
        pub th32_module_id: u32,
        pub cnt_threads: u32,
        pub th32_parent_process_id: u32,
        pub pc_pri_class_base: i32,
        pub dw_flags: u32,
        pub sz_exe_file: [u16; 260],
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn SetWindowPos(
            hwnd: isize,
            insert_after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        pub fn GetWindowLongW(hwnd: isize, n_index: i32) -> i32;
        pub fn SetWindowLongW(hwnd: isize, n_index: i32, dw_new_long: i32) -> i32;
        pub fn GetClassNameW(hwnd: isize, lp_class_name: *mut u16, n_max_count: i32) -> i32;
        pub fn GetWindowThreadProcessId(hwnd: isize, lpdw_process_id: *mut u32) -> u32;
        pub fn GetForegroundWindow() -> isize;
        pub fn GetAncestor(hwnd: isize, ga_flags: u32) -> isize;
        pub fn IsWindow(hwnd: isize) -> i32;
        pub fn IsWindowVisible(hwnd: isize) -> i32;
        pub fn SetForegroundWindow(hwnd: isize) -> i32;
        pub fn BringWindowToTop(hwnd: isize) -> i32;
        pub fn AllowSetForegroundWindow(process_id: u32) -> i32;
        pub fn SetWinEventHook(
            event_min: u32,
            event_max: u32,
            hmod_win_event_proc: isize,
            pfn_win_event_proc: unsafe extern "system" fn(
                isize,
                u32,
                isize,
                i32,
                i32,
                u32,
                u32,
            ),
            id_process: u32,
            id_thread: u32,
            dw_flags: u32,
        ) -> isize;
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetCurrentProcessId() -> u32;
        pub fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
        pub fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        pub fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        pub fn CloseHandle(handle: isize) -> i32;
        pub fn OpenProcess(desired_access: u32, inherit: i32, process_id: u32) -> isize;
        pub fn GetExitCodeProcess(handle: isize, exit_code: *mut u32) -> i32;
        pub fn AttachConsole(process_id: u32) -> i32;
        pub fn FreeConsole() -> i32;
        pub fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
        pub fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
    }
}

#[cfg(windows)]
fn apply_tool_window_hwnd(hwnd: isize) {
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_APPWINDOW: i32 = 0x0004_0000;
    const WS_EX_TOOLWINDOW: i32 = 0x0000_0080;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    unsafe {
        let ex = win32::GetWindowLongW(hwnd, GWL_EXSTYLE);
        let new_ex = (ex | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW;
        if new_ex == ex {
            return;
        }
        win32::SetWindowLongW(hwnd, GWL_EXSTYLE, new_ex);
        win32::SetWindowPos(
            hwnd,
            0,
            0,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

#[cfg(windows)]
fn apply_tool_window_style(wdw: &tauri::WebviewWindow) {
    let Ok(hwnd) = wdw.hwnd() else {
        return;
    };
    apply_tool_window_hwnd(hwnd.0 as isize);
}

#[cfg(windows)]
fn window_pid(hwnd: isize) -> u32 {
    let mut pid = 0u32;
    unsafe {
        win32::GetWindowThreadProcessId(hwnd, &mut pid);
    }
    pid
}

#[cfg(windows)]
fn window_class(hwnd: isize) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { win32::GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

#[cfg(windows)]
fn is_visible_toplevel(hwnd: isize) -> bool {
    unsafe {
        win32::IsWindow(hwnd) != 0
            && win32::IsWindowVisible(hwnd) != 0
            && win32::GetAncestor(hwnd, 2) == hwnd
    }
}

#[cfg(windows)]
fn foreground_is_ours() -> bool {
    unsafe {
        let fg = win32::GetForegroundWindow();
        fg != 0 && window_pid(fg) == win32::GetCurrentProcessId()
    }
}

#[cfg(windows)]
fn picker_is_showing() -> bool {
    let hwnd = PICKER_HWND.load(Ordering::SeqCst);
    hwnd != 0 && is_visible_toplevel(hwnd)
}

#[cfg(windows)]
fn parent_pid(pid: u32) -> Option<u32> {
    unsafe {
        let snap = win32::CreateToolhelp32Snapshot(0x2, 0);
        if snap == 0 || snap == -1 {
            return None;
        }
        let mut entry = std::mem::zeroed::<win32::ProcessEntry32W>();
        entry.dw_size = std::mem::size_of::<win32::ProcessEntry32W>() as u32;
        let mut found = None;
        if win32::Process32FirstW(snap, &mut entry) != 0 {
            loop {
                if entry.th32_process_id == pid {
                    found = Some(entry.th32_parent_process_id);
                    break;
                }
                if win32::Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        win32::CloseHandle(snap);
        found
    }
}

#[cfg(windows)]
fn clear_lifted_picker() {
    PICKER_HWND.store(0, Ordering::SeqCst);
}

/// Keep the Node-hosted `IFileOpenDialog` above Tauri without owning or disabling it.
#[cfg(windows)]
fn lift_foreign_dialog(hwnd: isize) {
    let already = PICKER_HWND.load(Ordering::SeqCst) == hwnd;
    PICKER_HWND.store(hwnd, Ordering::SeqCst);
    let foreground = unsafe { win32::GetForegroundWindow() } == hwnd;
    let plan = foreign_dialog_lift_plan(already, foreground);
    debug_assert!(!plan.assign_owner && !plan.disable_main);
    const HWND_TOPMOST: isize = -1;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_SHOWWINDOW: u32 = 0x0040;
    unsafe {
        win32::AllowSetForegroundWindow(0xFFFF_FFFF);
        if !plan.restack_topmost {
            return;
        }
        let mut flags = SWP_NOSIZE | SWP_NOMOVE;
        if plan.show_window {
            flags |= SWP_SHOWWINDOW;
        }
        if !plan.set_foreground {
            flags |= SWP_NOACTIVATE;
        }
        win32::SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
        // Both of these activate a top-level window, so a re-lift restacks and
        // stops there.
        if plan.set_foreground {
            win32::BringWindowToTop(hwnd);
            win32::SetForegroundWindow(hwnd);
        }
    }
}

/// Called only for windows the WinEvent hook has just seen appear.
///
/// The hook is desktop-wide, so this runs for every window every process
/// shows. `should_lift_foreign_window` owns the decision; the ordering here is
/// about its cost. `parent_pid` snapshots the whole process table once per
/// ancestor, so that walk sits behind the class check, which every window but
/// an actual dialog fails.
#[cfg(windows)]
fn consider_lift(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let class = window_class(hwnd);
    if class != DIALOG_CLASS {
        return;
    }
    if !is_visible_toplevel(hwnd) {
        return;
    }
    let wpid = window_pid(hwnd);
    let ours = unsafe { win32::GetCurrentProcessId() };
    if wpid == ours {
        return;
    }
    let in_tree = {
        let root = DSH_ROOT_PID.load(Ordering::SeqCst);
        root != 0 && pid_is_under_root_with(wpid, root, parent_pid)
    };
    if !should_lift_foreign_window(
        &class,
        wpid,
        ours,
        in_tree,
        foreground_is_ours(),
        true,
    ) {
        return;
    }
    lift_foreign_dialog(hwnd);
}

#[cfg(windows)]
unsafe extern "system" fn on_win_event(
    _hook: isize,
    event: u32,
    hwnd: isize,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    const OBJID_WINDOW: i32 = 0;
    const EVENT_SYSTEM_DIALOGSTART: u32 = 0x0010;
    const EVENT_OBJECT_DESTROY: u32 = 0x8001;
    const EVENT_OBJECT_SHOW: u32 = 0x8002;
    const EVENT_OBJECT_HIDE: u32 = 0x8003;
    if id_object != OBJID_WINDOW || hwnd == 0 {
        return;
    }
    if event == EVENT_OBJECT_HIDE || event == EVENT_OBJECT_DESTROY {
        if PICKER_HWND.load(Ordering::SeqCst) == hwnd {
            clear_lifted_picker();
        }
        return;
    }
    if event == EVENT_OBJECT_SHOW || event == EVENT_SYSTEM_DIALOGSTART {
        consider_lift(hwnd);
    }
}

#[cfg(windows)]
fn install_dialog_zorder_hook() {
    unsafe {
        win32::SetWinEventHook(0x8001, 0x8003, 0, on_win_event, 0, 0, 0);
        win32::SetWinEventHook(0x0010, 0x0010, 0, on_win_event, 0, 0, 0);
    }
}

#[cfg(windows)]
fn raise_win32(wdw: &tauri::WebviewWindow) {
    let Ok(hwnd) = wdw.hwnd() else {
        return;
    };
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_NOOWNERZORDER: u32 = 0x0200;
    unsafe {
        win32::SetWindowPos(
            hwnd.0 as isize,
            0,
            0,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

fn overlays_allowed(app: &AppHandle) -> bool {
    let withdrawn = app.state::<ShellState>().withdrawn.load(Ordering::SeqCst);
    if withdrawn {
        return false;
    }
    let Some(main) = app.get_webview_window("main") else {
        return false;
    };
    let visible = main.is_visible().unwrap_or(false);
    let minimized = main.is_minimized().unwrap_or(false);
    should_show_owned_overlay(minimized, visible)
}

fn hide_owned_overlays(app: &AppHandle) {
    for label in OWNED_OVERLAY_LABELS {
        if let Some(wdw) = app.get_webview_window(label) {
            let _ = wdw.hide();
        }
    }
}

fn handle_main_minimize_event(app: &AppHandle, is_minimized: bool) {
    let state = app.state::<ShellState>();
    if state.withdrawn.load(Ordering::SeqCst) {
        hide_owned_overlays(app);
        return;
    }
    let was_minimized = state.minimized.swap(is_minimized, Ordering::SeqCst);
    match minimize_transition(was_minimized, is_minimized) {
        MinimizeTransition::None => {}
        MinimizeTransition::Minimized => hide_owned_overlays(app),
        MinimizeTransition::Restored => {
            let _ = app.emit("app:restore", ());
        }
    }
}

/// Show `label` above sibling webviews (dsh) without recreating it.
#[tauri::command]
fn raise_overlay(app: AppHandle, label: String) {
    if !overlays_allowed(&app) {
        return;
    }
    // Reached on every focus change of the Harness webview, so it only reads
    // window state: rescanning the desktop for dialogs to lift from here is
    // what stole the caret out of the composer.
    #[cfg(windows)]
    if !allow_overlay_z_raise(picker_is_showing(), foreground_is_ours()) {
        return;
    }
    let Some(wdw) = app.get_webview_window(&label) else {
        return;
    };
    #[cfg(windows)]
    apply_tool_window_style(&wdw);
    let _ = wdw.show();
    #[cfg(windows)]
    raise_win32(&wdw);
}

/// True when the process token is elevated (launched via UAC / as administrator).
#[cfg(windows)]
fn is_elevated() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn CloseHandle(handle: isize) -> i32;
    }
    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
        fn GetTokenInformation(
            token: isize,
            class: i32,
            info: *mut std::ffi::c_void,
            info_len: u32,
            return_len: *mut u32,
        ) -> i32;
    }
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ELEVATION: i32 = 20;

    unsafe {
        let mut token: isize = 0;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: u32 = 0;
        let mut written: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TOKEN_ELEVATION,
            &mut elevation as *mut u32 as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
            &mut written,
        );
        CloseHandle(token);
        ok != 0 && elevation != 0
    }
}

#[cfg(windows)]
fn mark_elevated_title(app: &tauri::App) {
    const SUFFIX: &str = " (Elevated)";
    if !is_elevated() {
        return;
    }
    let Some(main) = app.get_webview_window("main") else {
        return;
    };
    let Ok(title) = main.title() else {
        return;
    };
    if title.ends_with(SUFFIX) {
        return;
    }
    let _ = main.set_title(&format!("{title}{SUFFIX}"));
}

fn hide_to_tray(app: &AppHandle) {
    app.state::<ShellState>()
        .withdrawn
        .store(true, Ordering::SeqCst);
    let _ = app.emit("app:withdraw", ());
    hide_owned_overlays(app);
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.hide();
    }
}

/// Quitting is not instant: the harness gets one graceful signal, then up to
/// `DSH_STOP_GRACE` to flush (this is where IDA writes its `*.i64`), then a
/// forced kill. That wait must not run on the event loop, or the progress
/// window put up to explain the delay would never paint.
static QUITTING: AtomicBool = AtomicBool::new(false);

const QUIT_WINDOW_LABEL: &str = "quitting";

/// True only for the click that owns the shutdown; later ones lose the race.
fn claim_quit(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}

fn quitting() -> bool {
    QUITTING.load(Ordering::SeqCst)
}

fn shutdown_dsh(app: &AppHandle) {
    let pid = {
        let state = app.state::<ServerState>();
        let mut guard = state.pid.lock().unwrap();
        guard.take()
    };
    remember_dsh_root_pid(None);
    reset_server_ready(app);
    if let Some(pid) = pid {
        kill_process_group(pid);
    }
}

fn show_quit_progress(app: &AppHandle) {
    if app.get_webview_window(QUIT_WINDOW_LABEL).is_some() {
        return;
    }
    let built = tauri::WebviewWindowBuilder::new(
        app,
        QUIT_WINDOW_LABEL,
        tauri::WebviewUrl::App("quit.html".into()),
    )
    .title("正在退出 DSH Desktop")
    // Tall enough that the subtitle wrapping to a second line — a different
    // UI font or DPI is enough to cause it — still is not clipped.
    .inner_size(420.0, 184.0)
    .center()
    .resizable(false)
    .minimizable(false)
    .maximizable(false)
    .decorations(false)
    .always_on_top(true)
    .focused(true)
    .build();
    if let Ok(wdw) = built {
        let _ = wdw.show();
    }
}

/// Show the progress window before withdrawing the shell, so the screen is
/// never empty between the click and the card.
fn begin_quit(app: &AppHandle) {
    if !claim_quit(&QUITTING) {
        return;
    }
    show_quit_progress(app);
    hide_to_tray(app);
    let app = app.clone();
    std::thread::spawn(move || {
        shutdown_dsh(&app);
        app.exit(0);
    });
}

fn restore_from_tray(app: &AppHandle) {
    if quitting() {
        return;
    }
    app.state::<ShellState>()
        .withdrawn
        .store(false, Ordering::SeqCst);
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.unminimize();
        let _ = main.show();
        let _ = main.set_focus();
    }
    let _ = app.emit("app:restore", ());
}

fn show_chrome_from_tray(app: &AppHandle) {
    if quitting() {
        return;
    }
    restore_from_tray(app);
    let _ = app.emit("chrome:show", ());
}

fn setup_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let show_chrome = MenuItem::with_id(
        app,
        TRAY_SHOW_CHROME_ID,
        "显示工具栏（CLI 日志）",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, TRAY_QUIT_ID, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_chrome, &quit])?;
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or("missing default window icon")?;
    TrayIconBuilder::with_id("tray")
        .icon(icon)
        .tooltip("DSH Desktop")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match tray_menu_action(event.id.as_ref()) {
            TrayAction::ShowChrome => show_chrome_from_tray(app),
            TrayAction::Quit => begin_quit(app),
            TrayAction::None => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                restore_from_tray(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    // Must be the first plugin: a second exe would also spawn `dsh web` on
    // port 3080 and fight the existing Node process.
    let mut builder = tauri::Builder::default();
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            restore_from_tray(app);
        }));
        builder = builder.plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .with_filter(persist_window_state_for)
                .build(),
        );
    }
    builder
        .manage(ServerState {
            pid: Mutex::new(None),
            launch_url: Mutex::new(url()),
            ready: AtomicBool::new(false),
        })
        .manage(ShellState {
            minimized: AtomicBool::new(false),
            withdrawn: AtomicBool::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            start_server,
            stop_server,
            restart_server,
            server_status,
            upgrade_dsh,
            dsh_version,
            sync_dsh_theme,
            restore_dsh_zoom,
            raise_overlay
        ])
        .setup(|app| {
            #[cfg(windows)]
            mark_elevated_title(app);
            #[cfg(windows)]
            {
                install_dialog_zorder_hook();
            }
            // Created hidden so restore can apply size/position/maximized first.
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
            }
            setup_tray(app)
        })
        .on_window_event(|window, event| {
            #[cfg(windows)]
            if let Ok(hwnd) = window.hwnd() {
                let hwnd = hwnd.0 as isize;
                if matches!(window.label(), "dsh" | "chrome-btn") {
                    apply_tool_window_hwnd(hwnd);
                }
            }
            match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    if window.label() == "main" {
                        api.prevent_close();
                        hide_to_tray(window.app_handle());
                    } else if window.label() == "chrome-btn" {
                        api.prevent_close();
                    }
                }
                tauri::WindowEvent::Resized(_)
                | tauri::WindowEvent::Moved(_)
                | tauri::WindowEvent::Focused(_)
                    if window.label() == "main" =>
                {
                    let minimized = window.is_minimized().unwrap_or(false);
                    handle_main_minimize_event(window.app_handle(), minimized);
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            match event {
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Reopen { .. } => restore_from_tray(app_handle),
                // A tray quit has already drained this off the event loop; any
                // other exit path (OS shutdown, task manager close) still needs
                // the tree stopped here.
                tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit => {
                    shutdown_dsh(app_handle)
                }
                _ => {}
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_uses_camel_case() {
        let settings = AppSettings {
            source: LaunchSource::Local,
            local_path: r"H:\github\deepseek-harness".to_string(),
            zoom: 1.25,
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(json.contains("\"source\":\"local\""));
        assert!(json.contains("\"localPath\""));
        let parsed: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source, LaunchSource::Local);
        assert_eq!(parsed.local_path, settings.local_path);
        assert_eq!(parsed.zoom, 1.25);
    }

    #[test]
    fn settings_written_before_zoom_existed_still_load() {
        let parsed: AppSettings =
            serde_json::from_str(r#"{"source":"npx","localPath":"H:\\repo"}"#).unwrap();
        assert_eq!(parsed.zoom, 1.0);
    }

    #[test]
    fn observed_zoom_divides_out_the_monitor_scale() {
        // 125% zoom on a 150% display, and the same zoom on a 100% display.
        assert_eq!(observed_zoom(Some(1.875), 1.5), 1.25);
        assert_eq!(observed_zoom(Some(1.25), 1.0), 1.25);
        // Chromium's 67% rung, which is 2/3 and needs rounding.
        assert_eq!(observed_zoom(Some(0.666_666_666), 1.0), 0.67);
    }

    #[test]
    fn observed_zoom_reads_an_unusable_probe_as_unzoomed() {
        assert_eq!(observed_zoom(None, 1.5), 1.0);
        assert_eq!(observed_zoom(Some(0.0), 1.5), 1.0);
        assert_eq!(observed_zoom(Some(f64::NAN), 1.5), 1.0);
        assert_eq!(observed_zoom(Some(1.5), 0.0), 1.0);
    }

    #[test]
    fn observed_zoom_stays_within_webview2s_range() {
        assert_eq!(observed_zoom(Some(40.0), 1.0), ZOOM_MAX);
        assert_eq!(observed_zoom(Some(0.01), 1.0), ZOOM_MIN);
    }

    #[test]
    fn the_first_reading_is_only_a_baseline() {
        assert!(!dpr_moved(None, 1.25));
        assert!(dpr_moved(Some(1.0), 1.25));
        assert!(!dpr_moved(Some(1.25), 1.25));
    }

    #[test]
    fn zoom_is_saved_only_when_the_user_moved_a_rung() {
        assert!(zoom_changed(1.0, 1.1));
        assert!(zoom_changed(1.25, 1.5));
        // Float noise around one reading must not rewrite the settings file.
        assert!(!zoom_changed(1.25, 1.25));
        assert!(!zoom_changed(1.25, 1.2501));
    }

    #[test]
    fn validate_local_repo_rejects_empty_and_missing() {
        assert!(validate_local_repo("").is_err());
        assert!(validate_local_repo("   ").is_err());
        assert!(validate_local_repo("Z:\\definitely-not-a-dsh-repo").is_err());
    }

    #[test]
    fn local_source_bin_is_cwd_independent() {
        let dir = PathBuf::from(r"H:\github\deepseek-harness");
        assert_eq!(
            local_source_bin(&dir),
            dir.join("apps").join("cli").join("src").join("bin.ts")
        );
    }

    #[test]
    fn looks_like_dsh_index_accepts_boot_html() {
        let res = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><script>window.__DSH_BOOT__={}</script></html>";
        assert!(looks_like_dsh_index(res.as_bytes()));
    }

    #[test]
    fn looks_like_dsh_index_rejects_startup_404() {
        let res = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";
        assert!(!looks_like_dsh_index(res.as_bytes()));
    }

    #[test]
    fn looks_like_dsh_index_rejects_unauthorized_index() {
        let res = "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n";
        assert!(!looks_like_dsh_index(res.as_bytes()));
        assert!(looks_like_unauthorized(res.as_bytes()));
    }

    #[test]
    fn parse_dsh_web_url_reads_token_launch_line() {
        assert_eq!(
            parse_dsh_web_url(
                "dsh web: http://127.0.0.1:3080/?token=XlJCK6TjsP-MQci6fvrdyeINwpj2iiVQ9YM28zF6rgw"
            ),
            Some(
                "http://127.0.0.1:3080/?token=XlJCK6TjsP-MQci6fvrdyeINwpj2iiVQ9YM28zF6rgw"
                    .to_string()
            )
        );
        assert_eq!(
            parse_dsh_web_url(
                "dsh web: http://127.0.0.1:4567/?token=test-token (LAN: http://192.168.1.5:4567/?token=test-token)"
            ),
            Some("http://127.0.0.1:4567/?token=test-token".to_string())
        );
        assert_eq!(
            parse_dsh_web_url("dsh web: http://127.0.0.1:3080"),
            Some("http://127.0.0.1:3080".to_string())
        );
        assert_eq!(
            parse_dsh_web_url(
                "dsh web: opening the default browser; pass --no-open to disable"
            ),
            None
        );
        assert!(parse_dsh_web_url("INFO:ida_pro_mcp.idalib_supervisor:Spawning").is_none());
    }

    #[test]
    fn looks_like_dsh_api_accepts_upgrade_required() {
        let res = "HTTP/1.1 426 Upgrade Required\r\nConnection: close\r\n\r\nupgrade required";
        assert!(looks_like_dsh_api(res.as_bytes()));
        assert!(!looks_like_dsh_api(b"HTTP/1.1 404 Not Found\r\n\r\n"));
    }

    #[test]
    fn web_launch_passes_no_open() {
        assert_eq!(DSH_WEB_ARGS[0], "web");
        assert!(
            DSH_WEB_ARGS.contains(&"--no-open"),
            "dsh web --no-open is the public switch that skips the default browser"
        );
    }

    #[test]
    fn session_export_anchor_matches_localized_header_labels() {
        assert!(session_export_anchor_matches("Session log"));
        assert!(session_export_anchor_matches("Session 日志"));
        assert!(session_export_anchor_matches("\n  Session 日志\n"));
        assert!(!session_export_anchor_matches("Session"));
        assert!(!session_export_anchor_matches("导出 Session"));
        for label in SESSION_EXPORT_LABELS {
            assert!(
                READ_DSH_THEME_JS.contains(label),
                "theme probe must keep {label} in sync with session_export_anchor_matches"
            );
        }
    }

    #[test]
    fn dsh_stop_waits_until_root_and_port_are_gone() {
        assert!(!dsh_tree_has_stopped(true, true));
        assert!(!dsh_tree_has_stopped(false, true));
        assert!(!dsh_tree_has_stopped(true, false));
        assert!(dsh_tree_has_stopped(false, false));
        assert!(
            DSH_STOP_GRACE > Duration::from_secs(5),
            "must outlast harness PROCESS_SHUTDOWN_TIMEOUT_MS so IDA can flush"
        );
    }

    #[test]
    fn pid_is_under_root_walks_ancestors() {
        let parent_of = |pid: u32| match pid {
            30 => Some(20),
            20 => Some(10),
            10 => Some(1),
            _ => None,
        };
        assert!(pid_is_under_root_with(30, 10, parent_of));
        assert!(pid_is_under_root_with(10, 10, parent_of));
        assert!(!pid_is_under_root_with(30, 99, parent_of));
        assert!(!pid_is_under_root_with(1, 10, parent_of));
    }

    #[test]
    fn overlay_z_raise_yields_to_foreign_picker_and_other_apps() {
        assert!(allow_overlay_z_raise(false, true));
        assert!(!allow_overlay_z_raise(true, true));
        assert!(!allow_overlay_z_raise(false, false));
        assert!(!allow_overlay_z_raise(true, false));
    }

    #[test]
    fn lift_dsh_worker_dialogs_but_not_our_own_windows() {
        assert!(!should_lift_foreign_window("#32770", 1, 1, false, true, true));
        assert!(!should_lift_foreign_window("#32770", 9, 1, false, true, false));
        assert!(should_lift_foreign_window("#32770", 9, 1, false, true, true));
        assert!(should_lift_foreign_window("#32770", 9, 1, true, false, true));
        assert!(
            !should_lift_foreign_window("Qt682QTQWindowIcon", 9, 1, true, false, true),
            "an IDA window under the harness tree is not a picker"
        );
        assert!(
            !should_lift_foreign_window("Chrome_WidgetWin_1", 9, 1, false, true, true),
            "only the dialog class is ever a folder picker"
        );
    }

    #[test]
    fn picker_lift_never_reparents_or_disables_a_dialog_already_in_show() {
        let first = foreign_dialog_lift_plan(false, false);
        assert!(
            !first.assign_owner,
            "GWLP_HWNDPARENT mid-Show cancels the native picker"
        );
        assert!(
            !first.disable_main,
            "EnableWindow(FALSE) on the shell also cancels Show(NULL)"
        );
        assert!(first.restack_topmost);
        assert!(first.show_window);
        assert!(first.set_foreground);

        let already_front = foreign_dialog_lift_plan(true, true);
        assert!(!already_front.assign_owner);
        assert!(!already_front.disable_main);
        assert!(!already_front.restack_topmost);
        assert!(!already_front.show_window);

        let lost_focus = foreign_dialog_lift_plan(true, false);
        assert!(!lost_focus.assign_owner);
        assert!(!lost_focus.disable_main);
        assert!(lost_focus.restack_topmost);
        assert!(
            !lost_focus.show_window,
            "re-ShowWindow on a live modal dialog can dismiss it"
        );
        assert!(
            !lost_focus.set_foreground,
            "re-grabbing the foreground is what pulled the caret out of the composer"
        );
    }

    #[test]
    fn minimize_transition_only_fires_on_edge() {
        assert_eq!(
            minimize_transition(false, false),
            MinimizeTransition::None
        );
        assert_eq!(
            minimize_transition(true, true),
            MinimizeTransition::None
        );
        assert_eq!(
            minimize_transition(false, true),
            MinimizeTransition::Minimized
        );
        assert_eq!(
            minimize_transition(true, false),
            MinimizeTransition::Restored
        );
    }

    #[test]
    fn owned_overlays_stay_hidden_while_main_is_minimized_or_hidden() {
        assert!(!should_show_owned_overlay(true, true));
        assert!(!should_show_owned_overlay(true, false));
        assert!(
            !should_show_owned_overlay(false, false),
            "hide-to-tray must not resurrect the frameless dsh overlay"
        );
        assert!(should_show_owned_overlay(false, true));
        assert_eq!(OWNED_OVERLAY_LABELS, ["dsh", "chrome-btn"]);
    }

    #[test]
    fn only_the_first_quit_click_owns_the_shutdown() {
        let flag = AtomicBool::new(false);
        assert!(claim_quit(&flag));
        assert!(!claim_quit(&flag), "a second click must not stop dsh twice");
    }

    #[test]
    fn quit_progress_window_is_neither_overlay_nor_persisted() {
        assert!(!persist_window_state_for(QUIT_WINDOW_LABEL));
        assert!(
            !OWNED_OVERLAY_LABELS.contains(&QUIT_WINDOW_LABEL),
            "withdrawing the shell would hide the progress window with it"
        );
    }

    #[test]
    fn tray_offers_a_toolbar_escape_hatch_next_to_quit() {
        assert_eq!(tray_menu_action(TRAY_SHOW_CHROME_ID), TrayAction::ShowChrome);
        assert_eq!(tray_menu_action(TRAY_QUIT_ID), TrayAction::Quit);
        assert_eq!(tray_menu_action("whatever"), TrayAction::None);
    }

    #[test]
    fn window_state_tracks_only_the_main_shell() {
        assert!(persist_window_state_for("main"));
        for label in OWNED_OVERLAY_LABELS {
            assert!(!persist_window_state_for(label));
        }
    }

    #[test]
    fn is_web_ready_requires_boot_html_and_api_route() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut started = false;
            for _ in 0..12 {
                let Ok((mut s, _)) = listener.accept() else { break };
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body: &[u8] = if !started {
                    started = true;
                    b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n"
                } else if req.contains("/api/events.host") {
                    b"HTTP/1.1 426 Upgrade Required\r\nConnection: close\r\n\r\nupgrade required"
                } else {
                    b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n<html>window.__DSH_BOOT__={}</html>"
                };
                let _ = s.write_all(body);
            }
        });
        assert!(!is_web_ready(port), "404 during plugin mount must not count as ready");
        assert!(
            is_web_ready(port),
            "boot HTML plus /api/events.host 426 must count as ready"
        );
    }
}
