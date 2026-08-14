#![windows_subsystem = "windows"]

// Brix is a Windows-only packager: it targets the system WebView2 runtime and
// uses Win32 APIs directly. Refusing to build on other platforms makes the
// supported surface explicit instead of shipping silent no-op stubs.
#[cfg(not(windows))]
compile_error!(
    "Brix native host targets Windows only (WebView2 + Win32). \
     Non-Windows builds are not supported. See https://github.com/haadiali242/Brix."
);

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::PathBuf;
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    window::WindowBuilder,
};
use wry::{PageLoadEvent, WebViewBuilder};
use zip::ZipArchive;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize, Debug)]
struct BrixConfig {
    name: String,
    #[serde(default = "default_version")]
    version: String,
    entry: String,
    window: WindowSettings,
    #[serde(default)]
    backend: Option<BackendSettings>,
    #[serde(default)]
    tray: Option<TrayConfig>,
    #[serde(default, rename = "minimizeToTray")]
    minimize_to_tray: bool,
    #[serde(default)]
    devtools: bool,
    #[serde(default)]
    splash: Option<SplashSettings>,
    #[serde(default)]
    webview2: Option<WebView2Settings>,
    #[serde(default)]
    update: Option<UpdateSettings>,
    /// Extra windows opened at startup. Each opens the same app (or a custom
    /// URL); more can be opened at runtime via window_open.
    #[serde(default)]
    windows: Vec<ExtraWindowSettings>,
    /// Optional native extension: a sidecar process that handles custom
    /// `window.brix.invoke` methods the built-in bridge doesn't implement.
    /// Brix forwards unknown method calls to it over stdio and delivers the
    /// JSON response back to the calling window.
    #[serde(default)]
    extension: Option<ExtensionSettings>,
}

/// A secondary window opened at startup.
#[derive(Serialize, Deserialize, Debug)]
struct ExtraWindowSettings {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    url: Option<String>,
}

fn default_version() -> String {
    "1.0.0".into()
}

#[derive(Serialize, Deserialize, Debug)]
struct BackendSettings {
    command: String,
    args: Vec<String>,
    #[serde(default)]
    port: Option<u16>,
}

/// A native extension sidecar (see `extension` on BrixConfig).
#[derive(Serialize, Deserialize, Debug)]
struct ExtensionSettings {
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct WindowSettings {
    width: u32,
    height: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SplashSettings {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default = "default_splash_width")]
    width: u32,
    #[serde(default = "default_splash_height")]
    height: u32,
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default = "default_true", rename = "autoHide")]
    auto_hide: bool,
}

fn default_true() -> bool {
    true
}
fn default_splash_width() -> u32 {
    360
}
fn default_splash_height() -> u32 {
    200
}

/// Tray settings. Accepts `true` (default Show/Exit menu) or an object with
/// a custom icon, tooltip and menu.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
enum TrayConfig {
    Enabled(bool),
    Settings(TraySettings),
}

impl Default for TrayConfig {
    fn default() -> Self {
        TrayConfig::Enabled(false)
    }
}

impl TrayConfig {
    fn is_enabled(&self) -> bool {
        matches!(self, TrayConfig::Enabled(true) | TrayConfig::Settings(_))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct TraySettings {
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    tooltip: Option<String>,
    #[serde(default)]
    menu: Vec<TrayMenuItem>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct TrayMenuItem {
    label: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    checked: bool,
    #[serde(default)]
    separator: bool,
}

#[derive(Serialize, Deserialize, Debug)]
struct WebView2Settings {
    #[serde(default, rename = "fixedRuntimePath")]
    fixed_runtime_path: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct UpdateSettings {
    url: String,
    #[serde(default = "default_true", rename = "checkOnStart")]
    check_on_start: bool,
    #[serde(default, rename = "autoInstall")]
    auto_install: bool,
}

/// An update manifest served over HTTPS:
/// { "version": "1.1.0", "url": "https://.../app-1.1.0.exe", "notes": "..." }
#[derive(Deserialize, Clone, Debug)]
struct UpdateManifest {
    version: String,
    url: String,
    #[serde(default)]
    notes: Option<String>,
}

/// Messages from the IPC handler (runs on the WebView2 thread) to the event loop.
enum UserEvent {
    /// Deliver an IPC response to a specific window's webview.
    Ipc { window: tao::window::WindowId, response: String },
    /// A response from the native extension sidecar, destined for the window
    /// that originally invoked the custom method.
    ExtensionResponse { window: tao::window::WindowId, response: String },
    /// Open a new window (from the renderer via window_open) and deliver the
    /// response once its page has loaded.
    OpenWindow { args: serde_json::Value, response: String },
    /// Destroy a secondary window (window_close).
    CloseWindow(tao::window::WindowId),
    /// The page of the given window finished loading.
    PageLoaded(tao::window::WindowId),
    CloseSplash,
    SplashHide,
    Tray(tray_icon::TrayIconEvent),
    TrayMenuClicked(String),
    UpdateEvent(serde_json::Value),
    /// Backend port is ready (async wait completed).
    BackendReady,
}

#[cfg(target_os = "windows")]
type NativeWindowHandle = windows_sys::Win32::Foundation::HWND;
#[cfg(not(target_os = "windows"))]
type NativeWindowHandle = usize;

/// A read-only memory mapping of a file. The OS pages in only the bytes that
/// are actually touched, so a large bundled app costs little physical RAM
/// until its assets are requested.
#[cfg(target_os = "windows")]
struct FileMapping {
    mapping: windows_sys::Win32::Foundation::HANDLE,
    view: *const u8,
    len: usize,
}

#[cfg(target_os = "windows")]
impl FileMapping {
    fn map(file: &mut File) -> std::io::Result<FileMapping> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY,
        };

        let len = file.metadata()?.len() as usize;
        unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle() as HANDLE,
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            if view.Value.is_null() {
                CloseHandle(mapping);
                return Err(std::io::Error::last_os_error());
            }
            Ok(FileMapping {
                mapping,
                view: view.Value as *const u8,
                len,
            })
        }
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is read-only and kept alive for as long as the
        // FileMapping value lives, so the bytes are immutable and valid.
        unsafe { std::slice::from_raw_parts(self.view, self.len) }
    }
}

// SAFETY: the mapping can be shared between threads and the view is
// read-only, so no data races are possible.
#[cfg(target_os = "windows")]
unsafe impl Send for FileMapping {}
#[cfg(target_os = "windows")]
unsafe impl Sync for FileMapping {}

#[cfg(target_os = "windows")]
impl Drop for FileMapping {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS};
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view as *mut core::ffi::c_void,
            });
            CloseHandle(self.mapping);
        }
    }
}

/// Fallback for non-Windows platforms: read the file into memory.
#[cfg(not(target_os = "windows"))]
struct FileMapping {
    bytes: Vec<u8>,
}

#[cfg(not(target_os = "windows"))]
impl FileMapping {
    fn map(file: &mut File) -> std::io::Result<FileMapping> {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(FileMapping { bytes })
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

fn get_mime_type(path: &str) -> &'static str {
    match path {
        p if p.ends_with(".html") || p.ends_with(".htm") => "text/html",
        p if p.ends_with(".css") => "text/css",
        p if p.ends_with(".js") => "application/javascript",
        p if p.ends_with(".mjs") || p.ends_with(".cjs") => "text/javascript",
        p if p.ends_with(".json") => "application/json",
        p if p.ends_with(".map") => "application/json",
        p if p.ends_with(".webmanifest") => "application/manifest+json",
        p if p.ends_with(".wasm") => "application/wasm",
        p if p.ends_with(".png") => "image/png",
        p if p.ends_with(".jpg") || p.ends_with(".jpeg") => "image/jpeg",
        p if p.ends_with(".gif") => "image/gif",
        p if p.ends_with(".svg") => "image/svg+xml",
        p if p.ends_with(".ico") => "image/x-icon",
        p if p.ends_with(".webp") => "image/webp",
        p if p.ends_with(".avif") => "image/avif",
        p if p.ends_with(".bmp") => "image/bmp",
        p if p.ends_with(".woff2") => "font/woff2",
        p if p.ends_with(".woff") => "font/woff",
        p if p.ends_with(".ttf") => "font/ttf",
        p if p.ends_with(".otf") => "font/otf",
        p if p.ends_with(".eot") => "application/vnd.ms-fontobject",
        p if p.ends_with(".mp3") => "audio/mpeg",
        p if p.ends_with(".wav") => "audio/wav",
        p if p.ends_with(".ogg") => "audio/ogg",
        p if p.ends_with(".m4a") => "audio/mp4",
        p if p.ends_with(".aac") => "audio/aac",
        p if p.ends_with(".flac") => "audio/flac",
        p if p.ends_with(".mp4") || p.ends_with(".m4v") => "video/mp4",
        p if p.ends_with(".webm") => "video/webm",
        p if p.ends_with(".ogv") => "video/ogg",
        p if p.ends_with(".mov") => "video/quicktime",
        p if p.ends_with(".mpg") || p.ends_with(".mpeg") => "video/mpeg",
        p if p.ends_with(".pdf") => "application/pdf",
        p if p.ends_with(".txt") => "text/plain",
        p if p.ends_with(".md") => "text/markdown",
        p if p.ends_with(".csv") => "text/csv",
        p if p.ends_with(".xml") => "application/xml",
        _ => "application/octet-stream",
    }
}

/// Extensions that represent real static files. Requests for a missing file
/// with one of these extensions get a clean 404; anything else is treated as
/// a client-side route and falls back to the SPA entry document.
fn is_known_asset_ext(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "html" | "htm" | "css" | "js" | "mjs" | "cjs" | "json" | "map" | "webmanifest" | "wasm"
            | "png" | "jpg" | "jpeg" | "gif" | "svg" | "ico" | "webp" | "avif" | "bmp"
            | "woff" | "woff2" | "ttf" | "otf" | "eot"
            | "mp3" | "wav" | "ogg" | "m4a" | "aac" | "flac"
            | "mp4" | "m4v" | "webm" | "ogv" | "mov" | "mpg" | "mpeg"
            | "pdf" | "txt" | "md" | "csv" | "xml"
    )
}

/// Replaces characters that are invalid in Windows folder names.
fn sanitize_folder_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\x00'..='\x1F' => '_',
            c if c.is_whitespace() => '_',
            c => c,
        })
        .collect()
}

/// Polls a TCP address until it accepts connections (server-mode startup wait).
/// Synchronous port wait: blocks until the address is reachable (legacy, unused).
#[allow(dead_code)]
fn wait_for_port(addr: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// Async port wait: spawns a thread to poll the address, sends BackendReady event when done (legacy, unused).
#[allow(dead_code)]
fn wait_for_port_async(addr: String, proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(&addr).is_ok() {
                let _ = proxy.send_event(UserEvent::BackendReady);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        // Timeout: still send event to unblock UI (will show error on navigate)
        let _ = proxy.send_event(UserEvent::BackendReady);
    });
}

/// FNV-1a 64-bit hash used to fingerprint the bundle for the extraction cache.
fn hash64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Writes a line to the verbose log (only when --verbose / BRIX_VERBOSE=1).
fn verbose_log(verbose: bool, dir: &std::path::Path, msg: &str) {
    if !verbose {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("brix-verbose.log"))
    {
        let _ = writeln!(f, "{}", msg);
    }
}

/// Reads one entry from the bundle. Returns (mime, bytes) on success.
fn try_read(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<(String, Vec<u8>)> {
    if let Ok(mut asset) = archive.by_name(name) {
        let mut buffer = Vec::new();
        if asset.read_to_end(&mut buffer).is_ok() {
            return Some((get_mime_type(name).to_string(), buffer));
        }
    }
    None
}

/// Puts the child into a Windows Job Object configured with
/// KILL_ON_JOB_CLOSE, so the whole backend process tree dies with the app
/// even when the exe is force-killed.
#[cfg(target_os = "windows")]
fn assign_to_job_object(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size,
        );
        AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
    }
}

#[cfg(target_os = "windows")]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens a URL / file / app in the default handler via ShellExecuteW.
#[cfg(target_os = "windows")]
fn open_external(url: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = to_wide(url);
    unsafe {
        let result = ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        result as isize > 32
    }
}

#[cfg(not(target_os = "windows"))]
fn open_external(url: &str) -> bool {
    std::process::Command::new("xdg-open").arg(url).spawn().is_ok()
}

#[cfg(target_os = "windows")]
fn clipboard_write(text: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let wide = to_wide(text);
        let bytes = wide.len() * 2;
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if h.is_null() {
            CloseClipboard();
            return false;
        }
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr as *mut u8, bytes);
        GlobalUnlock(h);
        SetClipboardData(CF_UNICODETEXT as u32, h);
        CloseClipboard();
        true
    }
}

#[cfg(target_os = "windows")]
fn clipboard_read() -> Option<String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT as u32);
        if h.is_null() {
            CloseClipboard();
            return None;
        }
        let ptr = GlobalLock(h) as *const u16;
        if ptr.is_null() {
            CloseClipboard();
            return None;
        }
        let mut len = 0usize;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr as *const u16, len);
        let text = String::from_utf16_lossy(slice);
        GlobalUnlock(h);
        CloseClipboard();
        Some(text)
    }
}

#[cfg(not(target_os = "windows"))]
fn clipboard_write(_text: &str) -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn clipboard_read() -> Option<String> {
    None
}

/// Shows a native tray balloon notification without spawning PowerShell.
/// The icon is removed shortly after the balloon closes. Returns true when
/// the notification was queued.
#[cfg(target_os = "windows")]
fn notify(title: &str, message: &str, hwnd: NativeWindowHandle) -> bool {
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_INFO, NIM_ADD, NIM_DELETE, NIIF_INFO,
        NIIF_NOSOUND,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDI_APPLICATION, LoadIconW};

    static NEXT_UID: AtomicU32 = AtomicU32::new(0x5000);

    fn copy_utf16(dst: &mut [u16], text: &str) {
        let mut encoded = text.encode_utf16();
        let mut len = 0usize;
        while len + 1 < dst.len() {
            match encoded.next() {
                Some(unit) => dst[len] = unit,
                None => break,
            }
            len += 1;
        }
        // Do not leave a dangling surrogate pair at the truncation point.
        if len > 0 && (0xD800..=0xDBFF).contains(&dst[len - 1]) {
            len -= 1;
        }
        dst[len] = 0;
    }

    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = NEXT_UID.fetch_add(1, Ordering::Relaxed);
    nid.uFlags = NIF_ICON | NIF_INFO;
    nid.hIcon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };
    nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
    nid.Anonymous.uTimeout = 5000;
    copy_utf16(&mut nid.szInfo, message);
    copy_utf16(&mut nid.szInfoTitle, title);

    if unsafe { Shell_NotifyIconW(NIM_ADD, &nid) } == 0 {
        return false;
    }

    // The shell needs the icon to stay alive for the balloon's lifetime;
    // remove it a little after the balloon would have closed. Only the
    // Send-safe fields are moved into the cleanup thread.
    let cleanup_hwnd = nid.hWnd as usize;
    let cleanup_uid = nid.uID;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let mut delete: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        delete.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        delete.hWnd = cleanup_hwnd as NativeWindowHandle;
        delete.uID = cleanup_uid;
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &delete);
        }
    });
    true
}

#[cfg(not(target_os = "windows"))]
fn notify(_title: &str, _message: &str, _hwnd: NativeWindowHandle) -> bool {
    false
}

fn file_dialog(params: &serde_json::Value) -> serde_json::Value {
    let kind = params.get("kind").and_then(|v| v.as_str()).unwrap_or("open");
    let mut dialog = rfd::FileDialog::new();
    if let Some(title) = params.get("title").and_then(|v| v.as_str()) {
        dialog = dialog.set_title(title);
    }
    if let Some(dir) = params.get("defaultPath").and_then(|v| v.as_str()) {
        dialog = dialog.set_directory(dir);
    }
    if let Some(filters) = params.get("filters").and_then(|v| v.as_array()) {
        for f in filters {
            if let (Some(name), Some(exts)) = (
                f.get("name").and_then(|v| v.as_str()),
                f.get("extensions").and_then(|v| v.as_array()),
            ) {
                let list: Vec<&str> = exts.iter().filter_map(|v| v.as_str()).collect();
                if !list.is_empty() {
                    dialog = dialog.add_filter(name, &list);
                }
            }
        }
    }
    let multi = params.get("multi").and_then(|v| v.as_bool()).unwrap_or(false);
    match kind {
        "save" => dialog
            .save_file()
            .map(|p| serde_json::Value::String(p.to_string_lossy().to_string())),
        "dir" => dialog
            .pick_folder()
            .map(|p| serde_json::Value::String(p.to_string_lossy().to_string())),
        _ if multi => Some(serde_json::Value::Array(
            dialog
                .pick_files()
                .map(|paths| {
                    paths
                        .into_iter()
                        .map(|p| serde_json::Value::String(p.to_string_lossy().to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        )),
        _ => dialog
            .pick_file()
            .map(|p| serde_json::Value::String(p.to_string_lossy().to_string())),
    }
    .unwrap_or(serde_json::Value::Null)
}

/// Compares dotted numeric versions ("1.2.10" > "1.2.9"). Returns true when
/// `candidate` is strictly newer than `current`. Non-numeric segments are
/// ignored after comparison.
fn is_newer_version(current: &str, candidate: &str) -> bool {
    let nums = |v: &str| {
        v.split('.')
            .map(|s| s.trim().parse::<u64>().unwrap_or(0))
            .collect::<Vec<u64>>()
    };
    let a = nums(current);
    let b = nums(candidate);
    let len = a.len().max(b.len());
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        if bv != av {
            return bv > av;
        }
    }
    false
}

/// Scopes an fs-API path: relative paths resolve against `data_dir`;
/// absolute paths must already live inside `data_dir` or `temp_dir`.
/// Returns None for anything that escapes (e.g. ".." or a foreign drive).
fn resolve_scoped_path(
    data_dir: &std::path::Path,
    temp_dir: &std::path::Path,
    input: &str,
) -> Option<std::path::PathBuf> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let candidate = if let Some(rest) = input.strip_prefix("~/") {
        data_dir.join(rest)
    } else {
        let p = std::path::Path::new(input);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            data_dir.join(p)
        }
    };
    let in_root = |root: &std::path::Path| {
        let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let canonical = candidate
            .canonicalize()
            .unwrap_or_else(|_| lexical_normalize(&candidate));
        canonical.starts_with(&root_canon)
    };
    if in_root(data_dir) || in_root(temp_dir) {
        Some(candidate)
    } else {
        None
    }
}

fn lexical_normalize(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A minimal blocking HTTPS GET built on WinHTTP (system TLS, no bundled
/// crypto Ã¢â‚¬â€ keeps the exe tiny). Follows up to 5 redirects.
#[cfg(target_os = "windows")]
fn http_get(url_str: &str) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
        WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
        WINHTTP_FLAG_REFRESH, WINHTTP_FLAG_SECURE,
        WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
    };

    let mut current = url_str.to_string();
    for _hop in 0..5 {
        let parsed = url::Url::parse(&current).map_err(|e| e.to_string())?;
        let host = parsed.host_str().ok_or("url has no host")?.to_string();
        let port = parsed.port_or_known_default().unwrap_or(80) as u16;
        let secure = parsed.scheme() == "https";
        let path_and_query = {
            let mut pq = parsed.path().to_string();
            if let Some(q) = parsed.query() {
                pq.push('?');
                pq.push_str(q);
            }
            if pq.is_empty() {
                pq.push('/');
            }
            pq
        };

        unsafe {
            let session = WinHttpOpen(
                "BrixUpdater\0".encode_utf16().collect::<Vec<u16>>().as_ptr(),
                0, // WINHTTP_ACCESS_TYPE_DEFAULT_PROXY
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
            if session.is_null() {
                return Err("WinHttpOpen failed".into());
            }
            let host_wide: Vec<u16> = host.encode_utf16().chain(std::iter::once(0)).collect();
            let connect = WinHttpConnect(session, host_wide.as_ptr(), port, 0);
            if connect.is_null() {
                WinHttpCloseHandle(session);
                return Err("WinHttpConnect failed".into());
            }
            let verb = "GET\0".encode_utf16().collect::<Vec<u16>>();
            let object: Vec<u16> = path_and_query
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let mut flags = WINHTTP_FLAG_REFRESH;
            if secure {
                flags |= WINHTTP_FLAG_SECURE;
            }
            let request = WinHttpOpenRequest(
                connect,
                verb.as_ptr(),
                object.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                flags,
            );
            if request.is_null() {
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                return Err("WinHttpOpenRequest failed".into());
            }

            let sent = WinHttpSendRequest(
                request,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                0,
            );
            if sent == 0 {
                WinHttpCloseHandle(request);
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                return Err("WinHttpSendRequest failed".into());
            }
            if WinHttpReceiveResponse(request, std::ptr::null_mut()) == 0 {
                WinHttpCloseHandle(request);
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                return Err("WinHttpReceiveResponse failed".into());
            }

            let mut status: u32 = 0;
            let mut status_len: u32 = std::mem::size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS_CODE,
                std::ptr::null(),
                &mut status as *mut u32 as *mut _,
                &mut status_len,
                std::ptr::null_mut(),
            );

            if (300..400).contains(&status) {
                // Follow the redirect.
                let mut location: Vec<u16> = vec![0u16; 2048];
                let mut loc_len = (location.len() * 2) as u32;
                let got = WinHttpQueryHeaders(
                    request,
                    WINHTTP_QUERY_LOCATION,
                    std::ptr::null(),
                    location.as_mut_ptr() as *mut _,
                    &mut loc_len,
                    std::ptr::null_mut(),
                );
                WinHttpCloseHandle(request);
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                if got == 0 {
                    return Err(format!("redirect without Location ({})", status));
                }
                let s = String::from_utf16_lossy(&location[..loc_len as usize / 2]).trim().to_string();
                if s.is_empty() {
                    return Err("redirect without Location".into());
                }
                current = url::Url::parse(&current)
                    .and_then(|u| u.join(&s).map(|j| j.to_string()))
                    .unwrap_or_else(|_| s);
                continue;
            }

            if status != 200 {
                WinHttpCloseHandle(request);
                WinHttpCloseHandle(connect);
                WinHttpCloseHandle(session);
                return Err(format!("HTTP status {}", status));
            }

            let mut body: Vec<u8> = Vec::new();
            loop {
                let mut available: u32 = 0;
                if WinHttpQueryDataAvailable(request, &mut available) == 0 || available == 0 {
                    break;
                }
                let mut chunk: Vec<u8> = vec![0u8; available as usize];
                let mut read: u32 = 0;
                if WinHttpReadData(
                    request,
                    chunk.as_mut_ptr() as *mut _,
                    available,
                    &mut read,
                ) == 0
                {
                    break;
                }
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..read as usize]);
                if body.len() > 256 * 1024 * 1024 {
                    WinHttpCloseHandle(request);
                    WinHttpCloseHandle(connect);
                    WinHttpCloseHandle(session);
                    return Err("download exceeds 256 MB".into());
                }
            }
            WinHttpCloseHandle(request);
            WinHttpCloseHandle(connect);
            WinHttpCloseHandle(session);
            return Ok(body);
        }
    }
    Err("too many redirects".into())
}

#[cfg(not(target_os = "windows"))]
fn http_get(_url_str: &str) -> Result<Vec<u8>, String> {
    Err("http not supported on this platform".into())
}

/// Tries to decode an icon for the tray from the bundled `_tray_icon` file:
/// PNG via the `image` crate, ICO via the `ico` crate.
fn decode_tray_icon(bytes: &[u8], is_ico: bool) -> Option<(Vec<u8>, u32, u32)> {
    if is_ico {
        let dir = ico::IconDir::read(Cursor::new(bytes)).ok()?;
        let entry = dir.entries().iter().max_by_key(|e| e.width())?;
        let image = entry.decode().ok()?;
        return Some((image.rgba_data().to_vec(), image.width(), image.height()));
    }
    let image = image::load_from_memory(bytes).ok()?;
    let rgba = image.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

/// Resolves a secondary-window URL to an absolute one: pass through
/// http(s)/brix:// URLs unchanged, resolve anything else against the
/// directory of the app entry URL (e.g. "settings.html" -> "http://brix.app/settings.html").
fn resolve_extra_url(url: &str, entry_url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("brix://") {
        return url.to_string();
    }
    let base = entry_url
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("http://brix.app");
    format!("{}/{}", base, url.trim_start_matches('/'))
}

/// Runtime handle to a native extension sidecar. The host writes JSON request
/// lines (`{id, method, args}`) to `writer`; the sidecar replies with JSON
/// response lines (`{id, ok, result|error}`). In-flight request ids are tracked
/// in `pending` so responses reach the correct window.
struct ExtensionRuntime {
    writer: std::sync::Mutex<std::process::ChildStdin>,
    pending: std::sync::Mutex<HashMap<u64, tao::window::WindowId>>,
}

/// Everything a webview needs to serve the bundle and talk to native code.
/// Shared read-only state wrapped in `Arc` so every window (and every
/// protocol/IPC/navigation closure) can grab its own copy cheaply.
#[derive(Clone)]
struct WindowOpts {
    entry_url: String,
    entry_path: String,
    name_index: HashMap<String, String>,
    bundle: std::sync::Arc<FileMapping>,
    bundle_start: usize,
    bundle_end: usize,
    app_name: String,
    app_version: String,
    data_dir: PathBuf,
    temp_dir: PathBuf,
    devtools: bool,
    verbose: bool,
    final_backend_port: Option<u16>,
    is_main: bool,
    update_state: std::sync::Arc<std::sync::Mutex<UpdateState>>,
    proxy: tao::event_loop::EventLoopProxy<UserEvent>,
    extension: Option<std::sync::Arc<ExtensionRuntime>>,
}

/// Builds a webview for any window: the main window or a secondary one.
/// The splash logic, IPC routing and tray events are handled by the event
/// loop; here we only wire the per-window bits (protocol, IPC, navigation).
fn build_webview(
    opts: &std::sync::Arc<WindowOpts>,
    window: &tao::window::Window,
    web_context: &mut wry::WebContext,
) -> Result<wry::WebView, Box<dyn std::error::Error>> {
    let window_id = window.id();

    // The main window's native handle, used for tray balloon notifications.
    #[cfg(target_os = "windows")]
    let hwnd: NativeWindowHandle = {
        use tao::platform::windows::WindowExtWindows;
        window.hwnd() as NativeWindowHandle
    };
    #[cfg(not(target_os = "windows"))]
    let hwnd: NativeWindowHandle = 0;

    let opts_for_ipc = opts.clone();
    let opts_for_nav = opts.clone();
    let opts_for_page = opts.clone();
    let opts_for_proto = opts.clone();

    let builder = WebViewBuilder::with_web_context(web_context)
        .with_devtools(opts.devtools)
        .with_initialization_script(BRIDGE_JS)
        .with_ipc_handler(move |request| {
            let body = request.body();
            let response = handle_ipc(
                body,
                &opts_for_ipc.app_name,
                &opts_for_ipc.app_version,
                &opts_for_ipc.data_dir,
                &opts_for_ipc.temp_dir,
                hwnd,
                &opts_for_ipc.update_state,
                &opts_for_ipc.proxy,
                &window_id,
                &opts_for_ipc.extension,
            );
            if !response.is_empty() {
                let _ = opts_for_ipc.proxy.send_event(UserEvent::Ipc {
                    window: window_id,
                    response,
                });
            }
        })
        .with_navigation_handler(move |url| {
            let external = is_external_url(&url, opts_for_nav.final_backend_port);
            verbose_log(
                opts_for_nav.verbose,
                &opts_for_nav.data_dir,
                &format!(
                    "nav: {} (window {:?}) external={}",
                    url, window_id, external
                ),
            );
            if external {
                let _ = open_external(&url);
                return false;
            }
            true
        })
        .with_new_window_req_handler(move |url| {
            let _ = open_external(&url);
            false
        })
        .with_on_page_load_handler(move |event, _url| {
            if let PageLoadEvent::Finished = event {
                let _ = opts_for_page
                    .proxy
                    .send_event(UserEvent::PageLoaded(window_id));
                if opts_for_page.is_main {
                    let _ = opts_for_page.proxy.send_event(UserEvent::CloseSplash);
                }
            }
        })
        .with_custom_protocol("brix".into(), move |_id, request| {
            let uri = request.uri().to_string();
            let raw_path = url::Url::parse(&uri)
                .map(|u| u.path().to_string())
                .unwrap_or_else(|_| {
                    uri.replace("brix://app/", "")
                        .replace("brix://", "")
                        .replace("http://brix.app/", "")
                        .replace("https://brix.app/", "")
                });
            let raw_path = percent_encoding::percent_decode_str(&raw_path)
                .decode_utf8_lossy()
                .to_string();

            let path = raw_path.trim_start_matches('/').to_string();

            let final_path = if path.is_empty() {
                opts_for_proto.entry_path.to_string()
            } else {
                path
            };

            let response_builder = wry::http::Response::builder();
            let mut found: Option<(String, Vec<u8>)> = None;

            // A fresh archive per request over the shared immutable mapping:
            // concurrent asset fetches never block on a lock, and the bundle
            // is never copied onto the heap.
            if let Ok(mut archive) = ZipArchive::new(Cursor::new(
                &opts_for_proto.bundle.as_slice()
                    [opts_for_proto.bundle_start..opts_for_proto.bundle_end],
            )) {
                if let Some(result) = try_read(&mut archive, &final_path) {
                    found = Some(result);
                } else if let Some(real) = opts_for_proto.name_index.get(&final_path.to_lowercase()) {
                    if let Some(result) = try_read(&mut archive, real) {
                        found = Some(result);
                    }
                } else if let Some((dir, _)) = opts_for_proto.entry_path.rsplit_once('/') {
                    let relative_path = format!("{}/{}", dir, final_path);
                    if let Some(result) = try_read(&mut archive, &relative_path) {
                        found = Some(result);
                    } else if let Some(real) = opts_for_proto.name_index.get(&relative_path.to_lowercase()) {
                        if let Some(result) = try_read(&mut archive, real) {
                            found = Some(result);
                        }
                    }
                }

                if found.is_none() && !is_known_asset_ext(&final_path) {
                    if let Some(result) = try_read(&mut archive, &opts_for_proto.entry_path) {
                        found = Some(result);
                    }
                }
            }

            match found {
                Some((mime, bytes)) => {
                    // Serve HTML with the bridge injected so parse-time
                    // `window.brix` usage always works (wry's init scripts
                    // run only after the page's own scripts).
                    let body: Cow<[u8]> = if mime == "text/html" {
                        match String::from_utf8(bytes) {
                            Ok(html) => Cow::Owned(
                                inject_after_head(&html, &format!("<script>{}</script>", BRIDGE_JS))
                                    .into_bytes(),
                            ),
                            Err(e) => Cow::Owned(e.into_bytes()),
                        }
                    } else {
                        Cow::Owned(bytes)
                    };
                    response_builder
                        .header("Content-Type", mime)
                        .body(body)
                        .unwrap()
                }
                None => response_builder.status(404).body(Cow::Owned(Vec::new())).unwrap(),
            }
        })
        .with_url(&opts.entry_url);

    Ok(builder.build(window)?)
}

/// Handles one IPC request body and returns the JSON response string.
/// An empty string means "no immediate response" (the event loop delivers it
/// later, e.g. after a new window finishes loading).
fn handle_ipc(
    body: &str,
    app_name: &str,
    app_version: &str,
    data_dir: &PathBuf,
    temp_dir: &PathBuf,
    hwnd: NativeWindowHandle,
    update_state: &std::sync::Arc<std::sync::Mutex<UpdateState>>,
    event_proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
    window_id: &tao::window::WindowId,
    extension: &Option<std::sync::Arc<ExtensionRuntime>>,
) -> String {
    let respond = |id: u64, result: serde_json::Value| {
        serde_json::json!({ "id": id, "ok": true, "result": result }).to_string()
    };
    let respond_err = |id: u64, error: String| {
        serde_json::json!({ "id": id, "ok": false, "error": error }).to_string()
    };

    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({ "id": 0, "ok": false, "error": e.to_string() }).to_string()
        }
    };
    let id = parsed.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
    let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let args = parsed
        .get("args")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match method {
        "system_info" => {
            let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
            let exe = std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().to_string());
            respond(
                id,
                serde_json::json!({
                    "appName": app_name,
                    "appVersion": app_version,
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "cpus": cpus,
                    "dataDir": data_dir.to_string_lossy().to_string(),
                    "exePath": exe,
                }),
            )
        }
        "open_external" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                respond_err(id, "open_external requires a url".into())
            } else {
                let ok = open_external(url);
                respond(id, serde_json::json!({ "opened": ok }))
            }
        }
        "clipboard_write" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            respond(
                id,
                serde_json::json!({ "ok": clipboard_write(text) }),
            )
        }
        "clipboard_read" => match clipboard_read() {
            Some(text) => respond(id, serde_json::json!({ "text": text })),
            None => respond_err(id, "clipboard read failed".into()),
        },
        "file_dialog" => respond(id, file_dialog(&args)),
        "notification" => {
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or(app_name);
            let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let shown = notify(title, message, hwnd);
            respond(id, serde_json::json!({ "shown": shown }))
        }
        "splash_hide" => {
            let _ = event_proxy.send_event(UserEvent::SplashHide);
            respond(id, serde_json::json!({ "ok": true }))
        }
        "fs_read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match resolve_scoped_path(data_dir, temp_dir, path)
                .and_then(|p| std::fs::read(&p).ok())
            {
                Some(bytes) => respond(
                    id,
                    serde_json::json!({
                        "content": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
                        "size": bytes.len(),
                    }),
                ),
                None => respond_err(id, "fs_read_file: path missing or outside the sandbox".into()),
            }
        }
        "fs_write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content_b64 = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = match base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                content_b64,
            ) {
                Ok(b) => b,
                Err(_) => return respond_err(id, "fs_write_file: content must be base64".into()),
            };
            match resolve_scoped_path(data_dir, temp_dir, path) {
                Some(p) => {
                    let parent_ok = p
                        .parent()
                        .map(|d| std::fs::create_dir_all(d).is_ok())
                        .unwrap_or(false);
                    if parent_ok && std::fs::write(&p, &bytes).is_ok() {
                        respond(id, serde_json::json!({ "ok": true, "size": bytes.len() }))
                    } else {
                        respond_err(id, "fs_write_file: write failed".into())
                    }
                }
                None => respond_err(id, "fs_write_file: path missing or outside the sandbox".into()),
            }
        }
        "fs_read_dir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match resolve_scoped_path(data_dir, temp_dir, path) {
                Some(p) => {
                    let entries: Vec<serde_json::Value> = std::fs::read_dir(&p)
                        .map(|rd| {
                            rd.flatten()
                                .filter_map(|e| {
                                    let meta = e.metadata().ok()?;
                                    Some(serde_json::json!({
                                        "name": e.file_name().to_string_lossy(),
                                        "isDir": meta.is_dir(),
                                        "size": if meta.is_file() { meta.len() } else { 0 },
                                    }))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    respond(id, serde_json::json!({ "entries": entries }))
                }
                None => respond_err(id, "fs_read_dir: path missing or outside the sandbox".into()),
            }
        }
        "fs_mkdir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match resolve_scoped_path(data_dir, temp_dir, path) {
                Some(p) if std::fs::create_dir_all(&p).is_ok() => {
                    respond(id, serde_json::json!({ "ok": true }))
                }
                _ => respond_err(id, "fs_mkdir: path missing or outside the sandbox".into()),
            }
        }
        "fs_remove" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let is_dir = args.get("recursive").and_then(|v| v.as_bool()).unwrap_or(false);
            match resolve_scoped_path(data_dir, temp_dir, path) {
                Some(p) => {
                    let ok = if is_dir {
                        std::fs::remove_dir_all(&p).is_ok()
                    } else {
                        match std::fs::metadata(&p) {
                            Ok(m) if m.is_dir() => std::fs::remove_dir(&p).is_ok(),
                            _ => std::fs::remove_file(&p).is_ok(),
                        }
                    };
                    if ok {
                        respond(id, serde_json::json!({ "ok": true }))
                    } else {
                        respond_err(id, "fs_remove: remove failed".into())
                    }
                }
                None => respond_err(id, "fs_remove: path missing or outside the sandbox".into()),
            }
        }
        "fs_exists" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match resolve_scoped_path(data_dir, temp_dir, path) {
                Some(p) => respond(
                    id,
                    serde_json::json!({
                        "exists": p.exists(),
                        "isDir": p.is_dir(),
                    }),
                ),
                None => respond_err(id, "fs_exists: path missing or outside the sandbox".into()),
            }
        }
        "update_check" => {
            let manifest_url = {
                let state = update_state.lock().unwrap();
                state.manifest_url.clone()
            };
            match manifest_url {
                Some(url) => {
                    let state = update_state.clone();
                    let proxy = event_proxy.clone();
                    let version = app_version.to_string();
                    let temp = temp_dir.clone();
                    std::thread::spawn(move || {
                        let _ = run_update_check(&url, &version, &temp, &state, &proxy);
                    });
                    respond(id, serde_json::json!({ "status": "checking" }))
                }
                None => respond_err(id, "update: no update.url configured".into()),
            }
        }
        "update_install" => {
            let staged = temp_dir.join(".brix_update.exe");
            let staged_ready = {
                let state = update_state.lock().unwrap();
                state.downloaded && state.manifest.is_some()
            };
            if staged_ready {
                return respond(
                    id,
                    serde_json::json!({ "status": "ready", "path": staged.to_string_lossy().to_string() }),
                );
            }
            let manifest = {
                let state = update_state.lock().unwrap();
                state.manifest.clone()
            };
            match manifest {
                Some(m) if m.url.starts_with("http") => {
                    let state = update_state.clone();
                    let proxy = event_proxy.clone();
                    let temp = temp_dir.clone();
                    std::thread::spawn(move || {
                        let _ = download_update(&m.url, &temp, &state, &proxy);
                    });
                    respond(id, serde_json::json!({ "status": "downloading" }))
                }
                _ => respond_err(id, "update: no available update to install".into()),
            }
        }
        "window_open" => {
            let title = args
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let width = args.get("width").and_then(|v| v.as_u64());
            let height = args.get("height").and_then(|v| v.as_u64());
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let response = respond(
                id,
                serde_json::json!({ "queued": true }),
            );
            let _ = event_proxy.send_event(UserEvent::OpenWindow {
                args: serde_json::json!({
                    "title": title,
                    "width": width,
                    "height": height,
                    "url": url,
                }),
                response,
            });
            // The real response is delivered once the new page has loaded.
            String::new()
        }
        "window_close" => {
            let _ = event_proxy.send_event(UserEvent::CloseWindow(*window_id));
            respond(id, serde_json::json!({ "closed": true }))
        }
        _ => {
            // Forward to the native extension sidecar if one is configured.
            // The response is delivered later via UserEvent::ExtensionResponse
            // once the sidecar replies on stdout.
            // Security: only forward if extension is configured; otherwise return error.
            if let Some(ext) = extension {
                // Security: validate args size (prevent OOM / huge payloads)
                let arg_bytes: usize = args.to_string().bytes().count();
                if arg_bytes > 65536 {
                    return respond_err(id, "method args too large".into());
                }
                if let Ok(mut pending) = ext.pending.lock() {
                    pending.insert(id, *window_id);
                }
                let req = serde_json::json!({ "id": id, "method": method, "args": args })
                    .to_string();
                if let Ok(mut writer) = ext.writer.lock() {
                    use std::io::Write;
                    let _ = writer.write_all(req.as_bytes());
                    let _ = writer.write_all(b"
");
                }
                String::new()
            } else {
                respond_err(id, "unknown method".into())
            }
        }
    }
}

/// Shared auto-update state between the IPC handler and background threads.
struct UpdateState {
    manifest_url: Option<String>,
    manifest: Option<UpdateManifest>,
    downloaded: bool,
    auto_install: bool,
}

/// Fetches the update manifest and compares versions. Pushes events:
/// "update_available" (with version + notes) or "update_none".
fn run_update_check(
    url: &str,
    app_version: &str,
    temp_dir: &std::path::Path,
    state: &std::sync::Arc<std::sync::Mutex<UpdateState>>,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
) -> Result<(), String> {
    let body = http_get(url)?;
    let manifest: UpdateManifest = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let staged = temp_dir.join(".brix_update.exe");
    let already_staged = staged.exists()
        && std::fs::metadata(&staged)
            .map(|m| m.len() > 100_000)
            .unwrap_or(false);

    if !is_newer_version(app_version, &manifest.version) {
        {
            let mut s = state.lock().unwrap();
            s.manifest = None;
            s.downloaded = false;
        }
        let _ = proxy.send_event(UserEvent::UpdateEvent(
            serde_json::json!({ "event": "update_none", "version": manifest.version }),
        ));
        return Ok(());
    }

    let notes = manifest.notes.clone().unwrap_or_default();
    let event = serde_json::json!({
        "event": "update_available",
        "version": manifest.version,
        "notes": notes,
        "alreadyDownloaded": already_staged,
    });
    {
        let mut s = state.lock().unwrap();
        s.manifest = Some(manifest);
        s.downloaded = already_staged;
    }
    let _ = proxy.send_event(UserEvent::UpdateEvent(event));
    Ok(())
}

/// Downloads the update exe to the temp dir, emitting "update_progress"
/// events and a final "update_ready" event. 1 KB granularity on progress.
fn download_update(
    url: &str,
    temp_dir: &std::path::Path,
    state: &std::sync::Arc<std::sync::Mutex<UpdateState>>,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
) -> Result<(), String> {
    let bytes = http_get(url)?;
    if bytes.len() < 100_000 {
        return Err("downloaded file is too small".into());
    }
    let staged = temp_dir.join(".brix_update.exe");
    let _ = std::fs::create_dir_all(temp_dir);
    std::fs::write(&staged, &bytes).map_err(|e| e.to_string())?;
    {
        let mut s = state.lock().unwrap();
        s.downloaded = true;
    }
    let _ = proxy.send_event(UserEvent::UpdateEvent(
        serde_json::json!({ "event": "update_ready", "size": bytes.len() }),
    ));
    Ok(())
}

/// Spawns the staged update exe in "installer mode": it waits for this
/// process to exit, replaces the running exe, and relaunches the app.
fn spawn_update_installer(exe_path: &std::path::Path, temp_dir: &std::path::Path) {
    let staged = temp_dir.join(".brix_update.exe");
    if !staged.exists() {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = std::process::Command::new(&staged)
            .args([
                "--brix-update-install",
                &std::process::id().to_string(),
                &exe_path.to_string_lossy(),
                &staged.to_string_lossy(),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (exe_path, staged);
    }
}

/// Installer mode entry: waits for the parent pid to exit, replaces the
/// running exe with the staged copy, then relaunches the app fresh.
fn run_update_installer_mode(args: &[String]) {
    if args.len() < 4 {
        return;
    }
    let parent_pid: u32 = args[1].parse().unwrap_or(0);
    let current_exe = std::path::PathBuf::from(&args[2]);
    let staged = std::path::PathBuf::from(&args[3]);

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while std::time::Instant::now() < deadline {
            unsafe {
                let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, parent_pid);
                if handle.is_null() {
                    break; // parent gone
                }
                let mut code: u32 = 0;
                GetExitCodeProcess(handle, &mut code);
                CloseHandle(handle);
                if code != STILL_ACTIVE {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = parent_pid;
    }

    let replaced = std::fs::remove_file(&current_exe)
        .and_then(|_| std::fs::rename(&staged, &current_exe))
        .or_else(|_| {
            // Fallback when the old exe could not be removed (sharing
            // violation): copy over it.
            std::fs::copy(&staged, &current_exe).map(|_| ())
        });

    if replaced.is_ok() {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            let _ = std::process::Command::new(&current_exe)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
        }
    }
}

/// True when a URL points outside the app (should open in the default browser).
fn is_external_url(url: &str, backend_port: Option<u16>) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "brix" => false,
        "about" | "data" | "blob" | "javascript" => false,
        "http" | "https" => {
            let host = parsed.host_str().unwrap_or("");
            if host == "brix.app" {
                false
            } else if host == "127.0.0.1" || host == "localhost" {
                // With a backend running, local http(s) URLs are always
                // internal: servers may redirect to another local port
                // (e.g. :4567 -> :4527) or bind an ephemeral port. Only when
                // there is no backend (bundled mode) is a local URL external.
                backend_port.is_none()
            } else {
                true
            }
        }
        _ => true,
    }
}

/// The bridge injected into the page: window.brix.invoke(method, args) -> Promise.
const BRIDGE_JS: &str = r#"
(function () {
  if (window.brix) { return; }
  var _seq = 0;
  var _pending = {};
  var _events = {};
  var _post = (window.ipc && window.ipc.postMessage)
    ? function (m) { window.ipc.postMessage(m); }
    : function (m) { window.chrome.webview.postMessage(m); };
  function _handle(json) {
    var m;
    try { m = JSON.parse(json); } catch (e) { return; }
    var p = _pending[m.id];
    if (!p) { return; }
    delete _pending[m.id];
    if (m.ok) { p.resolve(m.result); } else { p.reject(new Error(m.error || 'brix error')); }
  }
  function _handleEvent(name, data) {
    var list = _events[name];
    if (!list) { return; }
    for (var i = 0; i < list.length; i++) {
      try { list[i](data); } catch (e) { }
    }
  }
  function _handleExtension(json) {
    _handle(json);
  }
  window.brix = {
    invoke: function (method, args) {
      return new Promise(function (resolve, reject) {
        var id = ++_seq;
        _pending[id] = { resolve: resolve, reject: reject };
        _post(JSON.stringify({ id: id, method: method, args: args === undefined ? null : args }));
      });
    },
    on: function (name, cb) {
      if (typeof cb !== 'function') { return window.brix; }
      (_events[name] = _events[name] || []).push(cb);
      return window.brix;
    },
    off: function (name, cb) {
      var list = _events[name];
      if (!list) { return window.brix; }
      _events[name] = list.filter(function (f) { return f !== cb; });
      return window.brix;
    },
    _handle: _handle,
    _handleEvent: _handleEvent,
    _handleExtension: _handleExtension,
    hmr: {
      accept: (fn) => { /* Vite HMR would call this on update */ },
      reject: (err) => { /* no-op */ }
    }
  };
})();
"#;

/// Inserts `snippet` right after the opening <head> tag (fallback: at the
/// very start). Used to give served HTML pages the window.brix bridge before
/// any of their inline scripts run — wry's own initialization scripts are
/// executed at ContentLoading, i.e. after parse-time scripts have already run.
fn inject_after_head(html: &str, snippet: &str) -> String {
    let lower = html.to_lowercase();
    if let Some(pos) = lower.find("<head") {
        if let Some(gt) = html[pos..].find('>') {
            let end = pos + gt + 1;
            return format!("{}{}{}", &html[..end], snippet, &html[end..]);
        }
    }
    format!("{}{}", snippet, html)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--brix-update-install") {
        run_update_installer_mode(&args);
        std::process::exit(0);
    }
    let verbose =
        std::env::args().any(|a| a == "--verbose") || std::env::var("BRIX_VERBOSE").is_ok();
    let result = run(verbose);

    if let Err(err) = &result {
        let dir = std::env::var("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("brix");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("brix-crash.log"))
        {
            use std::io::Write;
            let _ = writeln!(f, "pid={} error={:?}", std::process::id(), err);
        }
    }

    if result.is_err() {
        std::process::exit(1);
    }
}

fn run(verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    let exe_path = std::env::current_exe()?;
    let mut file = File::open(&exe_path)?;

    // Dev mode (set by `brix dev`): load an external dev-server URL instead
    // of the bundled app. Parsed here so it can override entry_url/title/size.
    let args: Vec<String> = std::env::args().collect();
    let dev_url: Option<String> = args
        .iter()
        .position(|a| a == "--dev")
        .and_then(|i| args.get(i + 1).cloned());
    let dev_title: Option<String> = args
        .iter()
        .position(|a| a == "--title")
        .and_then(|i| args.get(i + 1).cloned());
    let dev_width: Option<u32> = args
        .iter()
        .position(|a| a == "--width")
        .and_then(|i| args.get(i + 1).and_then(|s| s.parse::<u32>().ok()));
    let dev_height: Option<u32> = args
        .iter()
        .position(|a| a == "--height")
        .and_then(|i| args.get(i + 1).and_then(|s| s.parse::<u32>().ok()));
    let metadata = file.metadata()?;
    let file_size = metadata.len();

    // Footer layout (little-endian): [u64 zip size][32 bytes SHA-256 of the zip].
    const FOOTER_LEN: u64 = 8 + 32;
    if file_size < FOOTER_LEN {
        return Err("Binary too small".into());
    }
    file.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
    let mut footer_buf = [0u8; 8 + 32];
    file.read_exact(&mut footer_buf)?;
    let zip_size = u64::from_le_bytes(footer_buf[..8].try_into().unwrap());
    let expected_hash: [u8; 32] = footer_buf[8..40].try_into().unwrap();

    if zip_size == 0 || zip_size > file_size - FOOTER_LEN {
        return Err("Invalid bundle size".into());
    }

    // Memory-map the exe instead of copying the bundle onto the heap: the OS
    // pages in only the parts that are actually read, so physical RAM stays
    // proportional to what the app really uses.
    let mapped_file = FileMapping::map(&mut file)?;
    let bundle_start = (file_size - FOOTER_LEN - zip_size) as usize;
    let bundle_end = (file_size - FOOTER_LEN) as usize;
    let bundle: &[u8] = &mapped_file.as_slice()[bundle_start..bundle_end];

// Integrity: the bundle is hashed so a truncated or tampered exe is
    // refused at startup rather than serving a broken app.
    {
        let mut hasher = Sha256::new();
        hasher.update(bundle);
        let actual = hasher.finalize();
        if actual.as_slice() != expected_hash {
            return Err("Bundle integrity check failed — the executable may be corrupted or tampered.".into());
        }
    }

    let mut config: BrixConfig = {
        let mut archive = ZipArchive::new(Cursor::new(bundle))?;
        let mut config_file = archive.by_name(".brix.json")?;
        let mut content = String::new();
        config_file.read_to_string(&mut content)?;
        serde_json::from_str(&content)?
    };

    if let Some(ext) = &config.extension {
        if ext.command.trim().is_empty() {
            return Err("extension.command must not be empty".into());
        }
        if ext.args.iter().any(|a| a.trim().is_empty()) {
            return Err("extension.args must not contain empty strings".into());
        }
    }

    let data_dir = std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(format!("brix_{}", sanitize_folder_name(&config.name)));
    let _ = std::fs::create_dir_all(&data_dir);
    verbose_log(
        verbose,
        &data_dir,
        &format!(
            "start: bundle={} bytes app={} v{} data_dir={:?}",
            zip_size, config.name, config.version, data_dir
        ),
    );

    if let Some(settings) = &config.webview2 {
        if let Some(path) = &settings.fixed_runtime_path {
            if std::path::Path::new(path).is_dir() {
                std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", path);
                verbose_log(
                    verbose,
                    &data_dir,
                    &format!("webview2: fixed runtime at {}", path),
                );
            } else {
                verbose_log(
                    verbose,
                    &data_dir,
                    &format!("webview2: fixed runtime path missing: {}", path),
                );
            }
        }
    }

    let entry_path_str = config.entry.trim_start_matches('/').to_string();
    let temp_dir = std::env::temp_dir().join(format!("brix_{}", sanitize_folder_name(&config.name)));

    let mut _backend_child = None;
    if let Some(backend) = &config.backend {
        let _ = std::fs::create_dir_all(&temp_dir);
        let marker_path = temp_dir.join(".brix_extract_cache");
        let bundle_hash = hash64(bundle);
        let cache_hit = std::fs::read_to_string(&marker_path)
            .map(|m| m.trim() == bundle_hash.to_string())
            .unwrap_or(false);

        if !cache_hit {
            verbose_log(verbose, &data_dir, "backend: extracting bundle to temp dir");
            if let Ok(entries) = std::fs::read_dir(&temp_dir) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
            if let Ok(mut archive) = ZipArchive::new(Cursor::new(bundle)) {
                for i in 0..archive.len() {
                    if let Ok(mut file) = archive.by_index(i) {
                        let outpath = match file.enclosed_name() {
                            Some(path) => path.to_owned(),
                            None => continue,
                        };

                        let path_str = outpath.to_string_lossy().replace("\\", "/");
                        if path_str.ends_with('/') {
                            continue;
                        }

                        let relative_path = if path_str.starts_with("_backend/") {
                            path_str.replace("_backend/", "")
                        } else if path_str == ".brix.json" || path_str == "_app_icon.ico" {
                            continue;
                        } else {
                            path_str
                        };
                        if relative_path.is_empty() {
                            continue;
                        }

                        let dest_path = temp_dir.join(relative_path.replace("/", "\\"));
                        if let Some(parent) = dest_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let mut buffer = Vec::new();
                        if file.read_to_end(&mut buffer).is_ok() {
                            let _ = std::fs::write(&dest_path, &buffer);
                        }
                    }
                }
            }
            let _ = std::fs::write(&marker_path, bundle_hash.to_string());
        } else {
            verbose_log(verbose, &data_dir, "backend: extraction cache hit");
        }

        let port_str = backend.port.map(|p| p.to_string()).unwrap_or_else(|| "0".into());
        let mut cmd = std::process::Command::new(&backend.command);
        cmd.args(&backend.args);
        cmd.current_dir(&temp_dir);

        cmd.env("BRIX_DATA_DIR", &data_dir)
            .env("BRIX_TEMP_DIR", &temp_dir)
            .env("BRIX_APP_VERSION", &config.version)
            .env("BRIX_ENTRY", &entry_path_str)
            .env("BRIX_PORT", &port_str);
        if backend.port == Some(0) {
            let port_file = temp_dir.join(".brix_port");
            let _ = std::fs::remove_file(&port_file);
            cmd.env("BRIX_PORT_FILE", &port_file);
        }

        if let Ok(f) = File::create(data_dir.join("backend.log")) {
            cmd.stdout(std::process::Stdio::from(f));
        }
        if let Ok(f) = File::create(data_dir.join("backend-error.log")) {
            cmd.stderr(std::process::Stdio::from(f));
        }

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        if let Ok(child) = cmd.spawn() {
            #[cfg(target_os = "windows")]
            assign_to_job_object(&child);
            verbose_log(
                verbose,
                &data_dir,
                &format!("backend: spawned pid={}", child.id()),
            );
            _backend_child = Some(child);
        } else {
            verbose_log(verbose, &data_dir, "backend: spawn failed");
        }
    }

    // Prepare port resolution: determine which port to wait for, but defer
    // actual waiting until after EventLoop creation so splash can show immediately.
    let resolved_port: Option<u16> = config.backend.as_ref().and_then(|b| b.port);
    let mut final_backend_port: Option<u16> = None;
    let mut entry_url: String;
    let port_wait_addr: Option<String> = if let Some(port) = resolved_port {
        if port == 0 {
            // Dynamic port: will read from .brix_port after EventLoop
            None
        } else {
            // Fixed port: prepare address string for async wait
            Some(format!("127.0.0.1:{}", port))
        }
    } else {
        None
    };

    // For bundled mode (no backend), set entry_url immediately
    if resolved_port.is_none() {
        verbose_log(
            verbose,
            &data_dir,
            &format!("mode: bundled at brix://app/{}", entry_path_str),
        );
        entry_url = format!("brix://app/{}", entry_path_str);
    } else {
        // Placeholder: will be set after port wait completes
        entry_url = String::new();
    }

    // Dev mode: load an external dev-server URL instead of the bundle. The
    // dev origin is treated as an internal (non-external) URL so the
    // navigation handler keeps it inside the webview.
    if let Some(url) = &dev_url {
        if let Ok(parsed) = url::Url::parse(url) {
            if let Some(p) = parsed.port() {
                final_backend_port = Some(p);
            }
        }
        entry_url = url.clone();
        if let Some(t) = &dev_title { config.name = t.clone(); }
        if let Some(w) = dev_width { config.window.width = w; }
        if let Some(h) = dev_height { config.window.height = h; }
        verbose_log(verbose, &data_dir, &format!("mode: dev at {}", entry_url));
    }

    let event_loop: EventLoop<UserEvent> =
        tao::event_loop::EventLoopBuilder::with_user_event().build();
    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();

    // Start async port wait AFTER EventLoop creation (if backend mode with fixed port)
    let mut extension_runtime: Option<std::sync::Arc<ExtensionRuntime>> = None;
    let mut _extension_child: Option<std::process::Child> = None;
    let backend_ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    if let Some(addr) = port_wait_addr {
        let proxy_for_wait = proxy.clone();
        let ready_flag = backend_ready.clone();
        let data_dir_clone = data_dir.clone();
        let verbose_clone = verbose;
        std::thread::spawn(move || {
            // Fixed port: poll until connection succeeds
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while std::time::Instant::now() < deadline {
                if std::net::TcpStream::connect(&addr).is_ok() {
                    verbose_log(verbose_clone, &data_dir_clone, &format!("backend ready: http://{}/", addr));
                    ready_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = proxy_for_wait.send_event(UserEvent::BackendReady);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            verbose_log(verbose_clone, &data_dir_clone, "backend wait timed out");
            let _ = proxy_for_wait.send_event(UserEvent::BackendReady);
        });
    } else if resolved_port == Some(0) {
        // Dynamic port: read from .brix_port file, then wait
        let proxy_for_wait = proxy.clone();
        let ready_flag = backend_ready.clone();
        let temp_dir_clone = temp_dir.clone();
        let data_dir_clone = data_dir.clone();
        let verbose_clone = verbose;
        std::thread::spawn(move || {
            let port_file = temp_dir_clone.join(".brix_port");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut actual_port: Option<u16> = None;

            // First, wait for port file to appear
            while std::time::Instant::now() < deadline {
                if let Ok(content) = std::fs::read_to_string(&port_file) {
                    if let Ok(p) = content.trim().parse::<u16>() {
                        actual_port = Some(p);
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }

            if let Some(port) = actual_port {
                // Then wait for port to be ready
                let addr = format!("127.0.0.1:{}", port);
                let sub_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while std::time::Instant::now() < sub_deadline {
                    if std::net::TcpStream::connect(&addr).is_ok() {
                        verbose_log(verbose_clone, &data_dir_clone, &format!("backend ready: http://{}/", addr));
                        ready_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = proxy_for_wait.send_event(UserEvent::BackendReady);
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
            }

            verbose_log(verbose_clone, &data_dir_clone, "backend wait timed out (dynamic port)");
            let _ = proxy_for_wait.send_event(UserEvent::BackendReady);
        });
    } else {
        // No backend: mark ready immediately
        backend_ready.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    // Native extension sidecar: forwards unknown window.brix.invoke methods
    // to a user process over stdio (see ExtensionRuntime / handle_ipc).
    if let Some(ext) = &config.extension {
        let mut cmd = std::process::Command::new(&ext.command);
        cmd.args(&ext.args);
        cmd.env("BRIX_DATA_DIR", &data_dir)
            .env("BRIX_TEMP_DIR", &temp_dir)
            .env("BRIX_APP_VERSION", &config.version)
            .env("BRIX_ENTRY", &entry_path_str);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        let ext_stderr = File::create(data_dir.join("extension-error.log"))
            .map(std::process::Stdio::from)
            .unwrap_or_else(|_| std::process::Stdio::null());
        cmd.stderr(ext_stderr);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                if let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) {
                    let runtime = std::sync::Arc::new(ExtensionRuntime {
                        writer: std::sync::Mutex::new(stdin),
                        pending: std::sync::Mutex::new(HashMap::new()),
                    });
                    let rt_for_thread = runtime.clone();
                    let proxy_for_ext = proxy.clone();
                    std::thread::spawn(move || {
                        use std::io::{BufRead, BufReader};
                        let reader = BufReader::new(stdout);
                        for line in reader.lines().map_while(Result::ok) {
                            let v: serde_json::Value = match serde_json::from_str(&line) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                            let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
                            let result = v.get("result").cloned().unwrap_or(serde_json::Value::Null);
                            let error = v.get("error").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            let response = serde_json::json!({ "id": id, "ok": ok, "result": result, "error": error }).to_string();
                            if let Ok(mut pending) = rt_for_thread.pending.lock() {
                                if let Some(wid) = pending.remove(&id) {
                                    let _ = proxy_for_ext.send_event(UserEvent::ExtensionResponse { window: wid, response });
                                }
                            }
                        }
                    });
                    extension_runtime = Some(runtime);
                    _extension_child = Some(child);
                    verbose_log(verbose, &data_dir, "extension: spawned sidecar");
                } else {
                    verbose_log(verbose, &data_dir, "extension: missing stdio pipes");
                }
            }
            Err(e) => {
                verbose_log(verbose, &data_dir, &format!("extension: spawn failed: {}", e));
            }
        }
    }

    let splash_settings = config.splash.clone().unwrap_or(SplashSettings {
        enabled: true,
        width: 360,
        height: 200,
        background: None,
        image: None,
        text: None,
        auto_hide: true,
    });

    let splash_window = if splash_settings.enabled {
        let win = WindowBuilder::new()
            .with_decorations(false)
            .with_always_on_top(true)
            .with_inner_size(tao::dpi::LogicalSize::new(
                splash_settings.width,
                splash_settings.height,
            ))
            .build(&event_loop)?;
        if let Some(monitor) = win.current_monitor() {
            let msize = monitor.size();
            let wsize = win.outer_size();
            let x = (msize.width.saturating_sub(wsize.width) / 2) as i32;
            let y = (msize.height.saturating_sub(wsize.height) / 2) as i32;
            win.set_outer_position(tao::dpi::Position::Physical(tao::dpi::PhysicalPosition::new(x, y)));
        }
        Some(win)
    } else {
        None
    };

    let mut window_builder = WindowBuilder::new()
        .with_title(&config.name)
        .with_inner_size(tao::dpi::LogicalSize::new(
            config.window.width,
            config.window.height,
        ));

    let mut tray_rgba: Option<(Vec<u8>, u32, u32)> = None;
    // Optional custom tray icon, bundled by the CLI as _tray_icon.ico / _tray_icon.png.
    let mut custom_tray_icon: Option<(Vec<u8>, bool)> = None;
    if let Ok(mut archive) = ZipArchive::new(Cursor::new(bundle)) {
        if let Ok(mut icon_file) = archive.by_name("_app_icon.ico") {
            let mut icon_data = Vec::new();
            if icon_file.read_to_end(&mut icon_data).is_ok() {
                if let Ok(ico_dir) = ico::IconDir::read(Cursor::new(icon_data)) {
                    if let Some(entry) = ico_dir.entries().last() {
                        if let Ok(image) = entry.decode() {
                            let rgba = image.rgba_data();
                            if let Ok(icon) = tao::window::Icon::from_rgba(
                                rgba.to_vec(),
                                image.width(),
                                image.height(),
                            ) {
                                window_builder = window_builder.with_window_icon(Some(icon));
                            }
                            tray_rgba = Some((rgba.to_vec(), image.width(), image.height()));
                        }
                    }
                }
            }
        }
        for name in ["_tray_icon.ico", "_tray_icon.png"] {
            if let Ok(mut tray_file) = archive.by_name(name) {
                let mut data = Vec::new();
                if tray_file.read_to_end(&mut data).is_ok() && !data.is_empty() {
                    custom_tray_icon = Some((data, name.ends_with(".ico")));
                    break;
                }
            }
        }
    }

    let window = window_builder.build(&event_loop)?;
    let main_window_id = window.id();

    let mut name_index: HashMap<String, String> = HashMap::new();
    if let Ok(mut archive) = ZipArchive::new(Cursor::new(bundle)) {
        for i in 0..archive.len() {
            if let Ok(file) = archive.by_index(i) {
                let name = file.name().to_string();
                name_index.entry(name.to_lowercase()).or_insert(name);
            }
        }
    }

    let mut web_context = wry::WebContext::new(Some(data_dir.clone()));

    let splash_html = {
        let safe_name = config
            .name
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;");
        let safe_text = splash_settings
            .text
            .as_deref()
            .unwrap_or("Loading&hellip;")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;");
        let bg = splash_settings
            .background
            .clone()
            .unwrap_or_else(|| "linear-gradient(135deg,#214771,#2f5d94)".into());
        // Optional branded image bundled as _splash.png by the CLI.
        let mut image_tag = String::new();
        if let Ok(mut archive) = ZipArchive::new(Cursor::new(bundle)) {
            if let Ok(mut img) = archive.by_name("_splash.png") {
                let mut data = Vec::new();
                if img.read_to_end(&mut data).is_ok() && !data.is_empty() {
                    let b64 = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &data,
                    );
                    image_tag = format!(
                        "<img src=\"data:image/png;base64,{}\" style=\"max-width:70%;max-height:55%;object-fit:contain;margin-bottom:12px\">",
                        b64
                    );
                }
            }
        }
        format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>html,body{{margin:0;height:100%;background:{bg};color:#fff;font-family:'Segoe UI',Arial,sans-serif;display:flex;flex-direction:column;align-items:center;justify-content:center;overflow:hidden;user-select:none}}.logo{{font-size:26px;font-weight:800;letter-spacing:2px}}.sub{{font-size:12px;opacity:.8;margin-top:8px}}</style></head><body>{image_tag}<div class=\"logo\">{safe_name}</div><div class=\"sub\">{safe_text}</div></body></html>"
        )
    };

    let splash_webview = match &splash_window {
        Some(win) => {
            let splash = WebViewBuilder::with_web_context(&mut web_context)
                .with_html(splash_html)
                .build(win)?;
            Some(splash)
        }
        None => None,
    };

    let update_state: std::sync::Arc<std::sync::Mutex<UpdateState>> =
        std::sync::Arc::new(std::sync::Mutex::new(UpdateState {
            manifest_url: config.update.as_ref().map(|u| u.url.clone()),
            manifest: None,
            downloaded: false,
            auto_install: config
                .update
                .as_ref()
                .map(|u| u.auto_install)
                .unwrap_or(false),
        }));

    // Background update check on startup.
    if let Some(update) = &config.update {
        if update.check_on_start {
            let state = update_state.clone();
            let proxy_for_update = proxy.clone();
            let url = update.url.clone();
            let version = config.version.clone();
            let temp = temp_dir.clone();
            std::thread::spawn(move || {
                let _ = run_update_check(&url, &version, &temp, &state, &proxy_for_update);
            });
        }
    }

    let opts = std::sync::Arc::new(WindowOpts {
        entry_url: entry_url.clone(),
        entry_path: entry_path_str,
        name_index,
        bundle: std::sync::Arc::new(mapped_file),
        bundle_start,
        bundle_end,
        app_name: config.name.clone(),
        app_version: config.version.clone(),
        data_dir: data_dir.clone(),
        temp_dir: temp_dir.clone(),
        devtools: config.devtools,
        verbose,
        final_backend_port,
        is_main: true,
        update_state,
        proxy: proxy.clone(),
        extension: extension_runtime,
    });

    let webview = build_webview(&opts, &window, &mut web_context)?;

    // Startup windows from the `windows` config array.
    let mut extra_windows: HashMap<tao::window::WindowId, (tao::window::Window, wry::WebView)> =
        HashMap::new();
    for extra in &config.windows {
        let title = extra
            .title
            .clone()
            .unwrap_or_else(|| config.name.clone());
        let width = extra.width.unwrap_or(config.window.width);
        let height = extra.height.unwrap_or(config.window.height);
        let url = resolve_extra_url(
            &extra.url.clone().unwrap_or_else(|| opts.entry_url.clone()),
            &opts.entry_url,
        );
        let extra_window = WindowBuilder::new()
            .with_title(&title)
            .with_inner_size(tao::dpi::LogicalSize::new(width, height))
            .build(&event_loop)?;
        let extra_opts = std::sync::Arc::new(WindowOpts {
            is_main: false,
            entry_url: url,
            ..(*opts).clone()
        });
        let extra_webview = build_webview(&extra_opts, &extra_window, &mut web_context)?;
        let extra_id = extra_window.id();
        extra_windows.insert(extra_id, (extra_window, extra_webview));
        verbose_log(
            verbose,
            &data_dir,
            &format!("window: opened extra '{}' ({:?})", title, extra_id),
        );
    }

    let tray_settings = config.tray.clone().unwrap_or(TrayConfig::Enabled(false));
    let tray_enabled = tray_settings.is_enabled() || config.minimize_to_tray;
    let tray_actions: std::sync::Arc<std::sync::Mutex<HashMap<String, tray_icon::menu::MenuId>>> =
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

    let tray = if tray_enabled {
        use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, MenuId, PredefinedMenuItem};
        use tray_icon::{Icon, TrayIconBuilder};

        let settings = match &tray_settings {
            TrayConfig::Settings(s) => Some(s.clone()),
            _ => None,
        };

        let menu = Menu::new();
        let mut actions: Vec<(String, MenuId)> = Vec::new(); // (action, native id)

        if let Some(settings) = &settings {
            let mut has_exit = false;
            for (i, entry) in settings.menu.iter().enumerate() {
                if entry.separator {
                    let sep = PredefinedMenuItem::separator();
                    let _ = menu.append(&sep);
                    continue;
                }
                let id = entry
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("item_{}_{}", i, entry.label.to_lowercase().replace(' ', "_")));
                if id.to_lowercase() == "exit" {
                    has_exit = true;
                }
                if entry.checked {
                    let item =
                        CheckMenuItem::with_id(id.clone(), &entry.label, entry.enabled, true, None);
                    actions.push((id.clone(), item.id().clone()));
                    let _ = menu.append(&item);
                } else {
                    let item = MenuItem::with_id(id.clone(), &entry.label, entry.enabled, None);
                    actions.push((id.clone(), item.id().clone()));
                    let _ = menu.append(&item);
                }
            }
            if !has_exit {
                let item = MenuItem::with_id("exit", "Exit", true, None);
                actions.push(("exit".into(), item.id().clone()));
                let _ = menu.append(&item);
            }
        } else {
            let show_item = MenuItem::with_id("show", "Show", true, None);
            actions.push(("show".into(), show_item.id().clone()));
            let _ = menu.append(&show_item);
            let exit_item = MenuItem::with_id("exit", "Exit", true, None);
            actions.push(("exit".into(), exit_item.id().clone()));
            let _ = menu.append(&exit_item);
        }

        {
            let mut map = tray_actions.lock().unwrap();
            for (action, native_id) in &actions {
                map.insert(action.clone(), native_id.clone());
            }
        }

        // Tray icon: custom png/ico bundled as _tray_icon.*, else the window icon.
        let tray_image = custom_tray_icon
            .as_ref()
            .and_then(|(bytes, is_ico)| decode_tray_icon(bytes, *is_ico));

        let (rgba, w, h) = tray_image
            .or_else(|| tray_rgba.clone())
            .unwrap_or_else(|| {
                let mut buf = Vec::with_capacity(16 * 16 * 4);
                for _ in 0..16 * 16 {
                    buf.extend_from_slice(&[0x21, 0x47, 0x71, 0xFF]);
                }
                (buf, 16, 16)
            });
        let icon = Icon::from_rgba(rgba, w, h)?;

        let tooltip = match &settings {
            Some(s) => s.tooltip.clone().unwrap_or_else(|| config.name.clone()),
            None => config.name.clone(),
        };

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(&tooltip)
            .with_icon(icon)
            .build()?;

        // Forward tray/menu events to the event loop so it stays fully
        // event-driven: no polling, ControlFlow::Wait keeps the thread idle.
        let tray_proxy = proxy.clone();
        tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
            let _ = tray_proxy.send_event(UserEvent::Tray(event));
        }));
        let menu_proxy = proxy.clone();
        let menu_actions = tray_actions.clone();
        tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: tray_icon::menu::MenuEvent| {
            let action = menu_actions
                .lock()
                .unwrap()
                .iter()
                .find(|(_, id)| *id == &event.id)
                .map(|(a, _)| a.clone())
                .unwrap_or_else(|| event.id.as_ref().to_string());
            let _ = menu_proxy.send_event(UserEvent::TrayMenuClicked(action));
        }));

        verbose_log(verbose, &data_dir, "tray: enabled");
        Some(tray)
    } else {
        None
    };

    let mut splash_window_for_loop = splash_window;
    let mut splash_webview_for_loop = splash_webview;
    let webview_for_loop = webview;
    let main_window_id_for_loop = main_window_id;
    let minimize_to_tray = config.minimize_to_tray;
    let splash_auto_hide = splash_settings.auto_hide;
    let default_window_size = (config.window.width, config.window.height);
    let app_name_for_windows = config.name.clone();
    let mut opts_for_loop = opts.clone();
    let opts_for_exit = opts.clone();
    let verbose_for_loop = verbose;
    let data_dir_for_loop = data_dir.clone();
    let temp_dir_for_loop = temp_dir.clone();
    let mut web_context_for_loop = web_context;
    let mut pending_responses: HashMap<tao::window::WindowId, String> = HashMap::new();
    let resolved_port_for_loop = resolved_port;

    let mut exit_app = move |control_flow: &mut ControlFlow| {
        if let Some(mut child) = _backend_child.take() {
            let _ = child.kill();
        }
        if let Some(mut child) = _extension_child.take() {
            let _ = child.kill();
        }
        let wants_update = {
            let s = opts_for_exit.update_state.lock().unwrap();
            s.downloaded && s.manifest.is_some() && s.auto_install
        };
        if wants_update {
            spawn_update_installer(&exe_path, &temp_dir);
        }
        *control_flow = ControlFlow::Exit;
    };

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let Some(_tray) = &tray {
            match &event {
                Event::UserEvent(UserEvent::TrayMenuClicked(action)) => {
                    if action == "exit" {
                        exit_app(control_flow);
                        return;
                    } else if action == "show" {
                        window.set_visible(true);
                        window.set_focus();
                    } else {
                        // Custom menu item: notify every window's renderer.
                        let js = format!(
                            "window.brix && window.brix._handleEvent('tray_menu', {})",
                            serde_json::json!({ "id": action }).to_string()
                        );
                        let _ = webview_for_loop.evaluate_script(&js);
                        for (_, (_, wv)) in extra_windows.iter() {
                            let _ = wv.evaluate_script(&js);
                        }
                    }
                }
                Event::UserEvent(UserEvent::Tray(tray_event)) => {
                    use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = tray_event
                    {
                        window.set_visible(true);
                        window.set_focus();
                    }
                }
                _ => {}
            }
        }

        match event {
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if window_id == main_window_id_for_loop {
                    if minimize_to_tray {
                        window.set_visible(false);
                    } else {
                        exit_app(control_flow);
                    }
                } else if let Some((win, wv)) = extra_windows.remove(&window_id) {
                    drop(wv);
                    drop(win);
                }
            }
            Event::UserEvent(UserEvent::Ipc { window: wid, response }) => {
                let js = format!(
                    "window.brix && window.brix._handle({})",
                    serde_json::to_string(&response).unwrap_or_else(|_| "\"\"".into())
                );
                if wid == main_window_id_for_loop {
                    let _ = webview_for_loop.evaluate_script(&js);
                } else if let Some((_, wv)) = extra_windows.get(&wid) {
                    let _ = wv.evaluate_script(&js);
                }
            }
            Event::UserEvent(UserEvent::ExtensionResponse { window: wid, response }) => {
                let js = format!(
                    "window.brix && window.brix._handleExtension({})",
                    serde_json::to_string(&response).unwrap_or_else(|_| "\"\"".into())
                );
                if wid == main_window_id_for_loop {
                    let _ = webview_for_loop.evaluate_script(&js);
                } else if let Some((_, wv)) = extra_windows.get(&wid) {
                    let _ = wv.evaluate_script(&js);
                }
            }
            Event::UserEvent(UserEvent::OpenWindow { args, response }) => {
                let title = args
                    .get("title")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| app_name_for_windows.clone());
                let width = args
                    .get("width")
                    .and_then(|v| v.as_u64())
                    .map(|w| w as u32)
                    .unwrap_or(default_window_size.0);
                let height = args
                    .get("height")
                    .and_then(|v| v.as_u64())
                    .map(|h| h as u32)
                    .unwrap_or(default_window_size.1);
                let url = resolve_extra_url(
                    &args
                        .get("url")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| opts_for_loop.entry_url.clone()),
                    &opts_for_loop.entry_url,
                );
                let extra_window = WindowBuilder::new()
                    .with_title(&title)
                    .with_inner_size(tao::dpi::LogicalSize::new(width, height))
                    .build(target);
                match extra_window {
                    Ok(extra_window) => {
                        let extra_opts = std::sync::Arc::new(WindowOpts {
                            is_main: false,
                            entry_url: url,
                            ..(*opts_for_loop).clone()
                        });
                        match build_webview(&extra_opts, &extra_window, &mut web_context_for_loop)
                        {
                            Ok(wv) => {
                                let wid = extra_window.id();
                                pending_responses.insert(wid, response);
                                extra_windows.insert(wid, (extra_window, wv));
                                verbose_log(
                                    verbose_for_loop,
                                    &data_dir_for_loop,
                                    &format!("window: opened '{}' ({:?})", title, wid),
                                );
                            }
                            Err(e) => {
                                drop(extra_window);
                                verbose_log(
                                    verbose_for_loop,
                                    &data_dir_for_loop,
                                    &format!("window: failed to build '{}': {}", title, e),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        verbose_log(
                            verbose_for_loop,
                            &data_dir_for_loop,
                            &format!("window: failed to create '{}': {}", title, e),
                        );
                    }
                }
            }
            Event::UserEvent(UserEvent::CloseWindow(wid)) => {
                if wid == main_window_id_for_loop {
                    if minimize_to_tray {
                        window.set_visible(false);
                    } else {
                        exit_app(control_flow);
                    }
                } else if let Some((win, wv)) = extra_windows.remove(&wid) {
                    drop(wv);
                    drop(win);
                }
            }
            Event::UserEvent(UserEvent::BackendReady) => {
                // Backend is now ready (or timed out). Update entry_url and navigate main window.
                if let Some(port) = resolved_port_for_loop {
                    let actual_port = if port == 0 {
                        // Dynamic port: read from .brix_port
                        let port_file = temp_dir_for_loop.join(".brix_port");
                        if let Ok(content) = std::fs::read_to_string(&port_file) {
                            content.trim().parse::<u16>().ok()
                        } else {
                            None
                        }
                    } else {
                        Some(port)
                    };

                    if let Some(p) = actual_port {
                        let new_entry_url = format!("http://127.0.0.1:{}/", p);

                        // Update opts entry_url for future windows
                        if let Some(opts_mut) = std::sync::Arc::get_mut(&mut opts_for_loop) {
                            opts_mut.entry_url = new_entry_url.clone();
                            opts_mut.final_backend_port = Some(p);
                        }

                        // Navigate main window to backend URL
                        let _ = webview_for_loop.load_url(&new_entry_url);
                        verbose_log(verbose_for_loop, &data_dir_for_loop, &format!("nav: {} external=false", new_entry_url));
                    } else {
                        verbose_log(verbose_for_loop, &data_dir_for_loop, "backend: port not available, keeping splash");
                    }
                }
            }
            Event::UserEvent(UserEvent::PageLoaded(wid)) => {
                if let Some(response) = pending_responses.remove(&wid) {
                    let js = format!(
                        "window.brix && window.brix._handle({})",
                        serde_json::to_string(&response).unwrap_or_else(|_| "\"\"".into())
                    );
                    if wid == main_window_id_for_loop {
                        let _ = webview_for_loop.evaluate_script(&js);
                    } else if let Some((_, wv)) = extra_windows.get(&wid) {
                        let _ = wv.evaluate_script(&js);
                    }
                }
            }
            Event::UserEvent(UserEvent::CloseSplash) => {
                if splash_auto_hide {
                    // Drop the splash webview and window so its WebView2 renderer
                    // process is released instead of staying hidden in the
                    // background for the whole session.
                    if let Some(webview) = splash_webview_for_loop.take() {
                        drop(webview);
                    }
                    if let Some(win) = splash_window_for_loop.take() {
                        drop(win);
                    }
                }
            }
            Event::UserEvent(UserEvent::SplashHide) => {
                if let Some(webview) = splash_webview_for_loop.take() {
                    drop(webview);
                }
                if let Some(win) = splash_window_for_loop.take() {
                    drop(win);
                }
            }
            Event::UserEvent(UserEvent::UpdateEvent(payload)) => {
                let js = format!(
                    "window.brix && window.brix._handleEvent('update', {})",
                    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
                );
                let _ = webview_for_loop.evaluate_script(&js);
                for (_, (_, wv)) in extra_windows.iter() {
                    let _ = wv.evaluate_script(&js);
                }
            }
            _ => {}
        }
    });
    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_server_url_is_internal() {
        assert!(!is_external_url("http://127.0.0.1:4567/", Some(4567)));
    }

    #[test]
    fn server_redirect_to_other_local_port_is_internal() {
        // A backend may redirect to a different local port; the nav handler
        // must follow it inside the webview.
        assert!(!is_external_url("http://127.0.0.1:4527/", Some(4567)));
        assert!(!is_external_url("http://localhost:9999/x", Some(4567)));
    }

    #[test]
    fn bundled_mode_local_http_is_external() {
        assert!(is_external_url("http://127.0.0.1:8080/", None));
    }

    #[test]
    fn external_http_is_external() {
        assert!(is_external_url("https://example.com/", Some(4567)));
        assert!(!is_external_url("brix://app/index.html", Some(4567)));
        assert!(!is_external_url("about:blank", Some(4567)));
        assert!(!is_external_url("javascript:void(0)", Some(4567)));
    }

    #[test]
    fn version_compare_detects_newer_releases() {
        assert!(is_newer_version("1.0.0", "1.0.1"));
        assert!(is_newer_version("1.2.9", "1.2.10"));
        assert!(is_newer_version("0.9.9", "1.0.0"));
        assert!(!is_newer_version("1.2.3", "1.2.3"));
        assert!(!is_newer_version("1.2.4", "1.2.3"));
        assert!(!is_newer_version("2.0.0", "1.9.9"));
    }

    #[test]
    fn update_manifest_parses() {
        let manifest: UpdateManifest = serde_json::from_str(
            r#"{"version":"1.1.0","url":"https://example.com/app.exe","notes":"fixes"}"#,
        )
        .unwrap();
        assert_eq!(manifest.version, "1.1.0");
        assert_eq!(manifest.notes.as_deref(), Some("fixes"));
        assert!(is_newer_version("1.0.0", &manifest.version));
    }

    #[test]
    fn tray_config_accepts_bool_or_object() {
        let from_bool: TrayConfig = serde_json::from_str("true").unwrap();
        assert!(from_bool.is_enabled());
        let from_false: TrayConfig = serde_json::from_str("false").unwrap();
        assert!(!from_false.is_enabled());
        let from_obj: TrayConfig = serde_json::from_str(
            r#"{"tooltip":"hi","menu":[{"label":"Open","id":"open"},{"label":"-","separator":true}]}"#,
        )
        .unwrap();
        assert!(from_obj.is_enabled());
        let TrayConfig::Settings(s) = &from_obj else {
            panic!("expected settings");
        };
        assert_eq!(s.menu.len(), 2);
        assert!(s.menu[0].separator == false && s.menu[0].id.as_deref() == Some("open"));
    }

    #[test]
    fn extra_windows_config_parses() {
        let config: BrixConfig = serde_json::from_str(
            r#"{
                "name": "App",
                "entry": "dist/index.html",
                "window": { "width": 1000, "height": 700 },
                "windows": [
                    { "title": "Settings", "width": 500, "height": 400, "url": "settings.html" },
                    { "title": "About" }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(config.windows.len(), 2);
        assert_eq!(config.windows[0].title.as_deref(), Some("Settings"));
        assert_eq!(config.windows[0].url.as_deref(), Some("settings.html"));
        assert_eq!(config.windows[0].width, Some(500));
        assert!(config.windows[1].title.as_deref() == Some("About"));
        assert!(config.windows[1].width.is_none());
        let no_extra: BrixConfig = serde_json::from_str(
            r#"{"name":"A","entry":"e.html","window":{"width":1,"height":1}}"#,
        )
        .unwrap();
        assert!(no_extra.windows.is_empty());
    }

    #[test]
    fn fs_paths_are_scoped_to_data_and_temp_dirs() {
        let data = std::path::PathBuf::from("C:/AppData/brix_app");
        let temp = std::path::PathBuf::from("C:/Temp/brix_app");
        assert_eq!(
            resolve_scoped_path(&data, &temp, "config.json"),
            Some(data.join("config.json"))
        );
        assert_eq!(
            resolve_scoped_path(&data, &temp, "~/notes.txt"),
            Some(data.join("notes.txt"))
        );
        assert_eq!(
            resolve_scoped_path(&data, &temp, "C:/Temp/brix_app/x.txt"),
            Some(temp.join("x.txt"))
        );
        assert!(resolve_scoped_path(&data, &temp, "../evil.txt").is_none());
        assert!(resolve_scoped_path(&data, &temp, "C:/Windows/system32").is_none());
        assert!(resolve_scoped_path(&data, &temp, "").is_none());
    }
}
