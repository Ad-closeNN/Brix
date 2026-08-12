![Brix](branding/banner.svg)

# Documentation

Welcome to the official technical manual for Brix. This guide covers the `.brix` configuration syntax, CLI commands, and production best practices.

> [!IMPORTANT]
> **Windows-only runtime.** Brix targets the system **WebView2** engine (Windows 10/11) and uses Win32 APIs directly; the native host will not compile on macOS or Linux. You can *build* Windows `.exe` files from any OS with the Node CLI (Brix ships a prebuilt `stub.exe`), but the resulting app runs only on Windows. Brix is intentionally not cross-platform — its wedge is the fastest path from an existing web app to a single Windows `.exe`.

---

## 🏗️ The .brix Syntax

The `.brix` file is a JSON configuration located in your project root. It defines how your application is bundled and how the native host behaves.

### Core Properties

| Property | Type | Description |
| :--- | :--- | :--- |
| `name` | `string` | **(Required)** The display name of your app and the resulting `.exe` filename. |
| `entry` | `string` | **(Required)** Path to your main HTML file relative to the project root. |
| `version` | `string` | The version of your app (e.g., `0.0.1`). |
| `description` | `string` | A short description shown in the Windows File Properties metadata. |
| `icon` | `string` | Path to a `.ico` file for desktop and taskbar branding. |

### 📦 Full-Stack Backend (Sidecars)

Brix can orchestrate a backend process (like Node.js, Python, or Go) alongside your frontend. 

> [!IMPORTANT]
> Backend files are securely extracted to a private directory in `%TEMP%` at runtime and are automatically cleaned up on exit.

```json
"backend": {
  "command": "node",
  "args": ["server/index.js"],
  "files": ["server/**/*", "package.json"]
}
```

### ⚛️ Framework Support

Brix features a **Deep Resolver** to support modern frameworks (React, Vite, TypeScript) without extra configuration.

- **Root-Relative Paths**: Support for `/assets/app.js` is automatic.
- **Deep Resolution**: Brix checks paths relative to your `entry` directory if absolute resolution fails.
- **Protocol**: All assets are served over the high-performance `brix://` protocol.
- **SPA History Routing**: Deep links and refresh work with React Router `BrowserRouter` / Vue Router history mode — unknown routes are rewritten to your entry document (missing files with extensions still 404).
- **Full MIME Coverage**: The bundle is served with correct content types for `.mjs`, `.wasm`, audio/video (mp3, wav, ogg, m4a, aac, flac, mp4, webm, mov...), `.pdf`, fonts (woff, woff2, ttf, otf, eot), `.webmanifest`, source maps, `.avif`, and more — `WebAssembly.instantiateStreaming` works out of the box.

### 🖥️ Server Mode (SSR — Next.js, Express, etc.)

For server-rendered apps, give your backend a `port`. Brix then spawns the
backend, **waits for the port to accept connections**, and loads the app from
`http://127.0.0.1:<port>/` instead of the bundled `brix://` assets — so
Next.js (`next start`), Express, and any SSR framework work normally.

```json
"backend": {
  "command": "node",
  "args": ["server.js"],
  "port": 3000
}
```

> [!NOTE]
> Without a `port`, the backend still runs as a hidden sidecar process,
> but the app itself loads from the bundled assets over `brix://`.

### 🪟 Window & UX Options

| Property | Type | Default | Description |
| :--- | :--- | :--- | :--- |
| `window.width` / `window.height` | `int` | `1000` / `700` | Initial window size in logical pixels. |
| `windows` | `array` | `[]` | Extra windows opened at startup. Each entry: `{ title, width, height, url }` (all optional — defaults come from the main window). `url` may be relative (resolved against the entry page) or absolute. |
| `splash.enabled` | `bool` | `true` | Show a branded splash window while the app boots. Set `false` to skip it. |
| `splash.width` / `splash.height` | `int` | `360` / `200` | Splash window size in logical pixels. |
| `splash.background` | `string` | gradient | CSS background of the splash window (any CSS color or gradient). |
| `splash.image` | `string` | — | Path to a PNG shown in the center of the splash window (bundled into the exe). |
| `splash.text` | `string` | `Loading…` | Caption text under the app name. |
| `splash.autoHide` | `bool` | `true` | Hide the splash when the page finishes loading. Set `false` and call `brix.invoke('splash_hide')` yourself. |
| `tray` | `bool` \| `object` | `false` | System-tray icon. `true` gives a default Show / Exit menu; an object adds an icon, tooltip, and custom menu (below). |
| `tray.icon` | `string` | app icon | Path to a PNG or ICO for the tray icon (bundled into the exe). |
| `tray.tooltip` | `string` | app name | Hover text on the tray icon. |
| `tray.menu` | `object[]` | Show / Exit | Custom right-click menu. `{ label, id?, enabled?, checked?, separator? }`. `id="show"` shows the window, `id="exit"` quits; any other id fires a `tray_menu` event (below). An Exit item is auto-appended if missing. |
| `minimizeToTray` | `bool` | `false` | The close button hides the window to the tray instead of quitting. |
| `devtools` | `bool` | `false` | Enable WebView2 developer tools in the packaged app. |

```json
"splash": { "enabled": true, "width": 480, "height": 260, "background": "#214771", "image": "branding/splash.png", "text": "Warming up…" },
"tray": {
  "icon": "branding/tray.png",
  "tooltip": "My App",
  "menu": [
    { "label": "Open", "id": "show" },
    { "label": "Settings…", "id": "open-settings" },
    { "label": "-", "separator": true },
    { "label": "Quit", "id": "exit" }
  ]
},
"minimizeToTray": true,
"devtools": false
```

**Multiple windows.** Any page in the bundle can open more windows at
runtime, and a `windows` array opens windows at startup:

```json
"windows": [
  { "title": "Settings", "width": 480, "height": 600, "url": "settings.html" },
  { "title": "About" }
]
```

```js
// Open a new window (relative or absolute URL, or omit for the entry page).
await window.brix.invoke('window_open', { title: 'Notes', width: 420, height: 300, url: 'notes.html' });
// Close the calling window. Closing the main window quits the app (or
// hides to tray when minimizeToTray is set).
await window.brix.invoke('window_close');
```

Every window gets its own `window.brix` bridge and can use the full API;
tray-menu and update events are broadcast to all windows.

When a menu item with a custom id is clicked, your app receives the event:

```js
window.brix.on('tray_menu', ({ id }) => {
  if (id === 'open-settings') window.brix.invoke('open_external', { url: 'https://example.com/settings' });
});
```

### 🪄 Native APIs (the `window.brix` bridge)

Your frontend gets a promise-based native bridge with zero SDK code:

```js
const info = await window.brix.invoke('system_info');
// { appName, appVersion, os, arch, cpus, dataDir, exePath }

await window.brix.invoke('open_external', { url: 'https://example.com' });
await window.brix.invoke('clipboard_write', { text: 'copied!' });
const { text } = await window.brix.invoke('clipboard_read');

// File dialogs + notifications
const picked = await window.brix.invoke('file_dialog', { dialog: 'open', title: 'Pick a file' });
await window.brix.invoke('notification', { title: 'Hi', message: 'Update ready' });

// Sandboxed fs: relative/~/absolute paths must stay inside the app's
// data dir (LOCALAPPDATA\brix_<app>) or its private temp dir.
await window.brix.invoke('fs_write_file', { path: 'settings.json', content: Buffer.from('{}').toString('base64') });
const { content } = await window.brix.invoke('fs_read_file', { path: 'settings.json' });
const { entries } = await window.brix.invoke('fs_read_dir', { path: '.' });
await window.brix.invoke('fs_mkdir', { path: 'data' });
await window.brix.invoke('fs_remove', { path: 'data/tmp.bin', recursive: false });
const { exists, isDir } = await window.brix.invoke('fs_exists', { path: 'settings.json' });
```

Events (tray menu, updates) arrive via `window.brix.on(name, callback)`;
all `invoke` calls return promises that reject with an `Error` on failure.
The fs sandbox deliberately rejects `..` escapes and foreign absolute paths —
write a local HTTP sidecar if you need full-disk access.

### 🔄 Auto-Update

```json
"update": {
  "url": "https://example.com/updates/manifest.json",
  "checkOnStart": true,
  "autoInstall": false
}
```

The manifest is a small JSON file that Brix fetches over HTTPS (WinHTTP,
system TLS — no extra runtime):

```json
{ "version": "1.1.0", "url": "https://example.com/downloads/my-app-1.1.0.exe", "notes": "Bug fixes" }
```

- `checkOnStart` (default `true`) — checks once at launch; an
  `update` event fires with `{ event: "update_available", version, notes }`.
- `autoInstall` (default `false`) — `true` installs on quit automatically;
  otherwise your UI calls `brix.invoke('update_install')` to download and
  stage, and the update is applied when the app exits (the new exe replaces
  the running one and relaunches).
- Events: `window.brix.on('update', (e) => …)` — `update_available`,
  `update_none`, `update_progress` (download in progress), `update_ready`.
- `brix.invoke('update_check')` re-checks on demand.
- The update URL must be HTTPS.

### 🧩 Plugins (build-time)

```json
"plugins": ["brixpack-plugin-license", "brixpack-plugin-minify"]
```

Plugins are plain Node modules resolved from your project's `node_modules`
(you install them with `npm i`). Each exports hook functions:

```js
module.exports = {
  name: 'my-plugin',
  hooks: {
    async preBundle({ config, files, backendFiles }) {
      // Mutate the file lists before they're compressed (e.g. drop files, add generated ones).
      files.push({ name: 'version.txt', realPath: '/abs/path/version.txt', size: 4 });
    },
    async transformFile({ name, content }) {
      // Return a Buffer/string to replace a file's content, or null to keep it.
      if (name.endsWith('.html')) return content.toString().replace('<title>', '<title> (v2)');
    },
    async postBuild({ config, exePath }) {
      // exePath is the final exe — verify, checksum, or publish it.
    }
  }
};
```

### 🔏 Code Signing

```json
"sign": {
  "enabled": true,
  "certFile": "certs/my-app.pfx",
  "certPassword": "secret",
  "timestamp": "http://timestamp.digicert.com",
  "algorithm": "SHA256"
}
```

After stitching, Brix runs `signtool sign /fd SHA256 /f <pfx> /p <password> /t <timestamp> <exe>`
(signtool must be in PATH — it ships with the Windows SDK). A custom
command array overrides the defaults: `"command": ["signtool", "sign", "/tr", "http://timestamp.digicert.com"]`.
If signing fails the build still succeeds with a warning — the exe is
otherwise complete and unsigned.

### 🧩 WebView2 Runtime

By default Brix uses the Evergreen WebView2 runtime bundled with
Windows 10/11. For offline or locked-down environments you can pin a fixed
runtime folder:

```json
"webview2": { "fixedRuntimePath": "C:\\WebView2\\runtime" }
```

---

## 🛠️ CLI Commands

### `brix init [project] [--force]`
Scans a project folder and writes a `.brix` config that **lists every folder
and root file** (as `include` patterns), with the entry HTML, an `.ico`
icon, and a common server file (`server.js`, `app.py`, …) detected
automatically. Everything is then editable in the JSON — add files, point
the icon somewhere else, switch the backend to server mode by adding a
`port`, etc. Use `--force` to overwrite an existing `.brix`.

> The full JSON syntax for `.brix` and `installer/installer.json` lives in
> **[syntax.md](syntax.md)**.

### `brix build [project] [--out <dir>] [--list]`
The one-command build. Point it at any project folder (or directly at a
`.brix` file) — it does **not** need to run from inside the project:
1. **Collects** the files matched by `include` / `exclude` (junctions and
   symlinks are dereferenced) — nothing is copied or staged.
2. **Compresses** them into a secure ZIP bundle.
3. **Branding**: Injects Version Info, HadesWorld Copyrights, and Icons.
4. **Stitching**: Appends the bundle to the native host with a high-security footer.

The standalone exe lands in `Brix_Works/` next to the project (or in the
directory given with `--out`), so your source tree stays completely clean.

Use `brix build --list` as a dry run: it prints every file that would be
bundled without building anything.

> [!NOTE]
> `brix init` is gone — `brix build` is a single step and leaves no staging
> folder behind.

### `brix init installer [project] [--type zip|inno|nsis|msi] [--files a,b] [--name <n>] [--appVersion <v>] [--outBase <base>]`
Interactive wizard that writes an installer configuration
(`installer/installer.json`) for a built app. It shows the **root folder
first** — pick whole folders to ship them with their structure preserved
(each file gets a `subDir` destination you can edit later), or single root
files. The built exe is always included, and the wizard detects which
toolchains are installed:

| Type | Output | Requires |
| :--- | :--- | :--- |
| `zip` | Portable folder zip (exe + assets) | nothing — works out of the box |
| `inno` | `setup.exe` (Inno Setup 6) | `iscc` in PATH or default install |
| `nsis` | `installer.exe` (NSIS 3) | `makensis` in PATH or default install |
| `msi` | `.msi` (WiX v3/v4) | `candle` + `light`, or the `wix` CLI |

All flags skip the interactive prompts, e.g.
`brix init installer --type inno --files "readme.md,assets/**"`.

### `brix make [zip|inno|nsis|msi] [--project <dir>] [--out <dir>] [--name <n>] [--appVersion <v>]`
Builds the installer defined by `installer/installer.json` (created by
`brix init installer`). Without a target argument the type from the config
is used. `zip` needs no external tools; the others generate the script and
run the toolchain (with a clear message if the tool is missing). Output
lands in `installer/dist/` unless `--out` is given.

---

## 💎 Production Best Practices

### Sidecar Management
- Ensure your backend binds to a stable port.
- Use environment variables to pass the port from your backend to the frontend.

### Performance
- Final binaries are typically **< 2.5MB**.
- Native **WebView2** integration ensures minimal CPU and RAM usage.

---

## 💎 Built by HadesWorld
For enterprise support and custom native host builds, visit:
[brix.hadesworld.com](https://brix.hadesworld.com) · [LinkedIn](https://www.linkedin.com/in/haadiali/) · [GitHub](https://github.com/haadiali242)

<p align="center">
  <i>© 2026 HaadiAli, founder of HadesWorld.</i>
</p>
