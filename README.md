![Brix](branding/banner.svg)

<p align="center">
  <b>Brix: Lightweight CLI tool that packages web apps into standalone Windows executables using native WebView2.</b>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Version-0.1.0-blue?style=for-the-badge" alt="Version">
  <img src="https://img.shields.io/badge/Platform-Windows-0078d7?style=for-the-badge&logo=windows" alt="Platform">
  <img src="https://img.shields.io/badge/License-MIT-green?style=for-the-badge" alt="License">
</p>

---

## 🚀 Overview

**Brix** is a high-performance CLI designed to transform modern web projects into lightweight, standalone Windows applications. By leveraging the native **WebView2** runtime, Brix eliminates the overhead of Electron while providing a professional, branded host for your code.

Features include:
- Custom protocol asset serving
- Embedded backend support
- SHA-256 verification
- Installer generation

### 🪟 Platform

Brix is a **Windows-only runtime** — the native host is built on the system **WebView2** engine and Win32 APIs, and the crate refuses to compile on any other OS (`#![cfg(not(windows))] compile_error!`). This is a deliberate stance, not a gap:

- **Build from anywhere.** The `brix` CLI is plain Node, and Brix ships a prebuilt `stub.exe`, so you can *produce* Windows `.exe` files from Windows, macOS, or Linux CI — only the resulting app runs on Windows.
- **Run on Windows 10/11.** The evergreen WebView2 runtime is already present on consumer Windows; for offline/locked-down machines pin it with `webview2.fixedRuntimePath`.

---

## 📁 File Directories & Project Information

When using Brix, your app's structure revolves around a `.brix` configuration file. After building, the compiled executable lands in a designated output folder.

### Runtime files

| Location | Contents |
|---|---|
| `%LOCALAPPDATA%\brix_<name>` | WebView2 user data, `backend.log` + `backend-error.log` (sidecar output) |
| `%LOCALAPPDATA%\brix\brix-crash.log` | Startup failures (also logged with `--verbose`) |
| `%TEMP%\brix_<name>` | Server-mode extraction + backend working directory |
| next to the exe | **nothing** — works when installed to read-only locations like `Program Files` |

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
| `brixpack init [project]` | Scan a project and write a `.brix` config. |
| `brixpack build [project]` | Bundle `project` into a standalone exe. |
| `brixpack build [project] --out <dir>` | Write the exe to `<dir>`. |
| `brixpack build [project] --list` | Dry run: print every file that would be bundled. |
| `brixpack init installer [project]` | Interactive wizard: pick an installer type. |
| `brixpack make [zip\|inno\|nsis\|msi]` | Build the installer. |

> 💡 Both `brixpack` and `brix` command names work.

---

## 📘 Documentation & References

Explore the full potential of Brix, including window customization and deep framework integration.

- 👉 **[JSON Syntax Reference](syntax.md)** — every key of `.brix` and `installer/installer.json`.
- 👉 **[Features & Use Cases](documentation.md)** — scenarios, comparisons (Electron/Tauri), and testing results.

---

## 💎 Built by HaadiAli

Brix is created by a solo developer, **HaadiAli**, building this for fun and optimized for the modern web.

[GitHub](https://github.com/haadiali242/Brix) · [NPM Registry](https://www.npmjs.com/package/brixpack)

<p align="right">
  <i>© 2026 HaadiAli (https://github.com/haadiali242)</i>
</p>
