![Brix](branding/banner.svg)

<p align="center">
  <b>Zero-SDK desktop packaging for the modern web.</b>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Version-0.0.1-blue?style=for-the-badge" alt="Version">
  <img src="https://img.shields.io/badge/Platform-Windows-0078d7?style=for-the-badge&logo=windows" alt="Platform">
  <img src="https://img.shields.io/badge/License-MIT-green?style=for-the-badge" alt="License">
</p>

---

## 🚀 Overview

**Brix** by **HadesWorld** is a high-performance CLI designed to transform modern web projects into lightweight, standalone Windows applications. By leveraging the native **WebView2** runtime, Brix eliminates the overhead of Electron while providing a professional, branded host for your code.

### ✨ Why Brix?

- **⚡ Elite Performance**: Sub-2MB native host with minimal memory footprint.
- **🔌 Zero-SDK**: Package any web project without modifying a single line of code.
- **📦 Full-Stack Ready**: Seamlessly orchestrate Node.js or Python backends as sidecar processes.
- **🛡️ Secure Bundling**: Your assets are securely bundled and extracted only at runtime.
- **💎 Pro Branding**: Injected icons, metadata, and HadesWorld-certified copyrights.

### 🪟 Platform

Brix is a **Windows-only runtime** — the native host is built on the system **WebView2** engine and Win32 APIs, and the crate refuses to compile on any other OS (`#![cfg(not(windows))] compile_error!`). This is a deliberate stance, not a gap:

- **Build from anywhere.** The `brix` CLI is plain Node, and Brix ships a prebuilt `stub.exe`, so you can *produce* Windows `.exe` files from Windows, macOS, or Linux CI — only the resulting app runs on Windows.
- **Run on Windows 10/11.** The evergreen WebView2 runtime is already present on consumer Windows; for offline/locked-down machines pin it with `webview2.fixedRuntimePath`.
- Brix does not aim to be cross-platform. If you need macOS/Linux builds, use Electron or Tauri; Brix's wedge is the fastest path from an existing web app to a single Windows `.exe`.

---

## 🛠️ Quick Start

Just two commands — Brix stages, bundles, brands, and stitches everything for you.

### 1. Install (once)
```bash
npm install -g brixpack
```

### 2. Build
From anywhere, point Brix at your project (the folder containing `.brix` — or pass the `.brix` file directly):

```bash
brixpack build my-project
```

Your branded, standalone `.exe` lands in `my-project\Brix_Works\` — your source folder stays completely untouched (no staging folders, no stray files).

| Command | What it does |
|---|---|
| `brixpack init [project]` | Scan a project and write a `.brix` config listing every folder and root file, with entry/icon/backend detected. |
| `brixpack build [project]` | Bundle `project` (folder or `.brix` file; defaults to the current directory) into a standalone exe. |
| `brixpack build [project] --out <dir>` | Write the exe to `<dir>` instead of the default `Brix_Works\`. |
| `brixpack build [project] --list` | Dry run: print every file that would be bundled, without building. |
| `brixpack init installer [project]` | Interactive wizard: pick an installer type (portable zip / Inno / NSIS / MSI) and which folders/files to ship; writes `installer/installer.json`. |
| `brixpack make [zip\|inno\|nsis\|msi]` | Build the installer (portable zip needs no tools; Inno/NSIS/MSI use the installed toolchains). |

> 💡 The old `brix` command name is installed as an alias too — both `brixpack` and `brix` work.
> `brixpack build` doesn't need to run inside the project — `brixpack build /path/to/app` works from any directory, which makes CI builds trivial.

### Example `.brix`
```json
{
  "name": "My Pro App",
  "version": "1.2.3",
  "entry": "dist/index.html",
  "icon": "icon.ico",
  "window": { "width": 1000, "height": 700 },
  "include": ["./**/*"],
  "exclude": ["node_modules", ".git", "BRIX-APP", "*.exe"],
  "backend": {
    "command": "node",
    "args": ["server.js"],
    "port": 4567,
    "files": ["server.js", "src/server/**/*.js"]
  }
}
```

---

## 📋 Configuration Reference

Every Brix app is configured by a `.brix` JSON file in the project root. All keys are optional except `name` and `entry`.

| Key | Type | Default | Description |
|---|---|---|---|
| `name` | string | — | **Required.** App name; used for the window title, the exe filename (`name` → `name.exe`, sanitized), the WebView2 data folder, and file metadata. Max 60 chars. |
| `version` | string | `1.0.0` | Version written into the exe's file metadata as `x.y.z.w` (e.g. `1.2.3` → `1.2.3.0`). |
| `entry` | string | — | **Required.** Path to the main HTML document, relative to the project root. Backslashes are normalized automatically. Must stay inside the project folder. |
| `icon` | string | package `icon.ico` | Path to a `.ico` file. Injected into the exe resource (taskbar icon) and used as the window icon. Non-`.ico` files warn. |
| `window.width` / `window.height` | int | `1000` / `700` | Initial window size in logical pixels. Positive integers only. |
| `windows` | object[] | `[]` | Extra windows opened at startup: `{ title, width, height, url }` (all optional). `url` can be relative (resolved against the entry page). |
| `include` | string[] | `["./**/*"]` | Glob patterns for files bundled into the app. Paths escaping the project are skipped with a warning. |
| `exclude` | string[] | (see below) | Extra glob patterns excluded **in addition to** the built-in defaults (`node_modules`, `.git`, `BRIX-APP`, `Brix_Works`, `.brix`, `*.exe`, `*.log`, `*.WebView2`, `temp_*.zip`). Patterns without glob magic (e.g. `node_modules`) exclude everything below them too; basename patterns (`*.exe`) match at every depth. |
| `backend.command` | string | — | Executable for the sidecar backend (e.g. `node`, `python`). |
| `backend.args` | string[] | `[]` | Arguments passed to the backend. `.js` arguments are bundled automatically as a fallback when `backend.files` is absent. |
| `backend.port` | int | — | **Enables server mode.** When set, the app waits for `127.0.0.1:<port>` and loads `http://127.0.0.1:<port>/` instead of the bundled `brix://` app. Range 1–65535. |
| `backend.files` | string[] | — | Glob patterns of backend source files to bundle. Files land in the runtime temp folder; the `_backend/` prefix used at bundle time is stripped at runtime. |
| `splash.enabled` | bool | `true` | Show a branded splash window while the app boots. Set `false` to skip it. |
| `splash.width` / `splash.height` | int | `360` / `200` | Splash window size in logical pixels. |
| `splash.background` | string | gradient | CSS background of the splash window (any CSS color or gradient). |
| `splash.image` | string | — | Path to a PNG shown in the center of the splash window (bundled into the exe). |
| `splash.text` | string | `Loading…` | Caption text under the app name. |
| `splash.autoHide` | bool | `true` | Hide the splash when the page finishes loading; `false` = hide it manually via `brix.invoke('splash_hide')`. |
| `tray` | bool \| object | `false` | System-tray icon. `true` = default Show / Exit menu; object = `{ icon, tooltip, menu: [{ label, id, enabled?, checked?, separator? }] }` with custom menu (see docs). |
| `minimizeToTray` | bool | `false` | The close button hides the window to the tray instead of quitting. |
| `devtools` | bool | `false` | Enable WebView2 developer tools in the packaged app. |
| `webview2.fixedRuntimePath` | string | — | Path to a fixed WebView2 runtime folder (for offline / locked-down environments). |
| `update.url` | string | — | HTTPS URL of the update manifest `{ version, url, notes }`. |
| `update.checkOnStart` | bool | `true` | Check for updates once at launch (fires an `update` event). |
| `update.autoInstall` | bool | `false` | Install the staged update automatically when the app quits. |
| `plugins` | string[] | — | Build-time plugin packages (resolved from the project's `node_modules` or relative paths). Hooks: `preBundle`, `transformFile`, `postBuild`. |
| `sign` | object | — | Code signing via signtool: `{ enabled, certFile, certPassword, timestamp, algorithm }` or a custom `{ command: [...] }`. |
| `update` | object | — | Auto-update: `{ url, checkOnStart, autoInstall }` (see docs). |

> **Multiple windows:** any page can open more windows at runtime with
> `window.brix.invoke('window_open', { title, width, height, url })` and
> close the calling one with `window.brix.invoke('window_close')`. Each
> window gets its own bridge; tray/update events broadcast to all windows.

### How the app runs

- **Bundled mode (default)** — assets are served from memory through the custom `brix://` protocol. The page origin is `http://brix.app/` internally, so relative fetches behave exactly like a normal web server.
- **Server mode** — when `backend.port` is set, the whole bundle is extracted to `%TEMP%\brix_<name>` and the backend is spawned with that as its working directory. The app waits for the port (15s) and loads the backend URL. Extraction is cached — a `.brix_extract_cache` marker is kept, and re-extraction is skipped while the bundle hash matches.

### Routing rules (bundled mode)

1. Exact path lookup, then **case-insensitive** lookup (Windows semantics).
2. Paths relative to the entry directory (e.g. `dist/`) are tried next.
3. Missing paths with a **known static-asset extension** (`.png`, `.js`, `.woff2`, …) return `404`.
4. Everything else falls back to the **entry document** (SPA history-mode routing), so deep links and refreshes work with React Router / Vue Router.

Filenames containing spaces or special characters work: they are percent-encoded on the wire and decoded to the real zip entry name.

### Runtime files

| Location | Contents |
|---|---|
| `%LOCALAPPDATA%\brix_<name>` | WebView2 user data, `backend.log` + `backend-error.log` (sidecar output) |
| `%LOCALAPPDATA%\brix\brix-crash.log` | Startup failures (also logged with `--verbose`) |
| `%TEMP%\brix_<name>` | Server-mode extraction + backend working directory |
| next to the exe | **nothing** — works when installed to read-only locations like `Program Files` |

Run the exe with `--verbose` (or set `BRIX_VERBOSE=1`) to have it write a
`brix-verbose.log` startup trace into the data folder.

---

## 📘 Documentation

Explore the full potential of Brix, including window customization and deep framework integration.

- 👉 **[JSON syntax reference](syntax.md)** — every key of `.brix` and `installer/installer.json`.
- 👉 **[Full Documentation](https://brix.hadesworld.com/)**

---

## 🧪 Testing

The native host ships with Rust unit tests covering the navigation policy
(`is_external_url`), the update engine (version compare, manifest parsing),
tray config parsing, and the fs sandbox:

```bash
npm test    # cargo test on src/stub
```

---

## 💎 Built by HadesWorld
Brix is a proud product of **HadesWorld**, founded by **HaadiAli**. Join our community for updates and support.

[brix.hadesworld.com](https://brix.hadesworld.com) · [LinkedIn](https://www.linkedin.com/in/haadiali/) · [GitHub](https://github.com/haadiali242)

<p align="right">
  <i>© 2026 HaadiAli, founder of HadesWorld.</i>
</p>
