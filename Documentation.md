# Brix Documentation

Welcome to the official documentation for Brix. This guide covers use cases, features, comparison against Electron and Tauri, and testing results.

---

## 🌟 Features

Brix is built to provide an optimal experience for packaging web applications:

- **⚡ Elite Performance**: Sub-2MB native host with minimal memory footprint.
- **🔌 Zero-SDK**: Package any web project without modifying a single line of code.
- **📦 Full-Stack Ready**: Seamlessly orchestrate Node.js, Python, or Go backends as sidecar processes.
- **🛡️ Secure Bundling**: Your assets are securely bundled and extracted only at runtime.
- **💎 Pro Branding**: Injected icons and metadata.
- **🔄 Auto-Update**: Built-in support for checking and downloading updates automatically.
- **🔏 Code Signing**: Out-of-the-box signtool integration for secure distribution.
- **⚛️ Framework Support**: Built-in deep resolver supporting React, Vite, Vue Router, Next.js, and more natively without extra configuration.

---

## 💡 Usecase Scenarios

Brix shines in a variety of application scenarios:

### 1. Single Page Applications (SPAs)
For typical React, Vue, or Angular applications, Brix uses **Bundled mode**. Assets are served from memory through the custom `brix://` protocol. The internal page origin is `http://brix.app/`, so relative fetches act exactly like a normal web server.

### 2. Server-Side Rendered (SSR) Apps
For apps using Next.js, Nuxt, or Express, configure Brix in **Server mode**. By specifying `backend.port`, Brix extracts the bundle to a temporary directory, spawns the backend, waits for the port to become active, and loads the local server URL seamlessly.

### 3. Desktop Tools with Native Interactions
Leverage the `window.brix` bridge for native integrations:
- Reading/writing to the local filesystem
- Triggering system notifications
- Clipboard operations and external URL handling
- Opening and managing multiple windows

---

## ⚖️ Comparison: Brix vs. Electron vs. Tauri

### 🆚 Brix vs. Electron
- **Size**: Electron ships a bundled version of Chromium and Node.js, leading to huge app sizes (~150MB+). Brix uses the OS-native WebView2, keeping the final executable under **2.5MB**.
- **Memory Footprint**: Brix consumes significantly less RAM because it shares the WebView2 runtime with the OS.
- **Integration**: While Electron gives you full Node.js access in the main process, Brix separates concerns—frontend runs in a secure WebView, and backends run as optional sidecars.

### 🆚 Brix vs. Tauri
- **Ecosystem**: Tauri requires Rust tooling and ecosystem knowledge for advanced configurations. Brix is built purely for web developers—configuration is 100% JSON and CLI commands are Node-based.
- **Cross-Platform**: Tauri is cross-platform, while Brix is **Windows-only**. Brix’s goal is to be the absolute fastest and easiest way to get a Windows `.exe` out of a web app.

---

## 🧪 Testing Results

The native host ships with Rust unit tests ensuring the stability and security of the Brix runtime. 

Testing covers:
- **Navigation Policy**: `is_external_url` ensures that only authorized domains load inside the app.
- **Update Engine**: Accurate version comparisons and manifest parsing.
- **Tray Configurations**: Proper parsing and fallback mechanisms.
- **FS Sandbox**: Strict file system access boundaries protecting local environments.

To run the internal unit tests for the stub:
```bash
cargo test --manifest-path src/stub/Cargo.toml
```

---

## 🛠️ Native APIs (window.brix)

Your frontend gets a promise-based native bridge with zero SDK code required to initialize:

```js
// System Info
const info = await window.brix.invoke('system_info');

// File dialogs + notifications
const picked = await window.brix.invoke('file_dialog', { dialog: 'open', title: 'Pick a file' });
await window.brix.invoke('notification', { title: 'Hello', message: 'Update ready' });

// Sandboxed fs
await window.brix.invoke('fs_write_file', { path: 'settings.json', content: Buffer.from('{}').toString('base64') });
const { content } = await window.brix.invoke('fs_read_file', { path: 'settings.json' });
```
