# Brix Syntax Reference

Two JSON files drive a Brix project:

1. **`.brix`** — the app configuration file.
2. **`installer/installer.json`** — the installer configuration file.

---

## 1. `.brix` — The App Configuration

The `.brix` file configures your application's bundle and runtime behavior.

### 🌟 Core Properties
| Key | Type | Default | Description |
|---|---|---|---|
| `name` | string | — | **Required.** App name used for the window title, `.exe` filename, and file metadata. |
| `version` | string | `"1.0.0"` | Written into the exe's file metadata. |
| `entry` | string | — | **Required.** Path to the main HTML document, relative to the project root. |
| `icon` | string | `icon.ico` | Path to a `.ico` file. Injected into the exe resource. |

### 📦 Bundling & Files
| Key | Type | Default | Description |
|---|---|---|---|
| `include` | string[] | `["./**/*"]` | Glob patterns for files bundled into the app. |
| `exclude` | string[] | (built-in) | Extra glob patterns excluded in addition to defaults like `node_modules` and `.git`. |
| `plugins` | string[] | — | Build-time plugin packages. Hooks: `preBundle`, `transformFile`, `postBuild`. |

### 🪟 Window Settings
| Key | Type | Default | Description |
|---|---|---|---|
| `window.width` / `height` | int | `1000` / `700` | Initial window size in logical pixels. |
| `windows` | object[] | `[]` | Extra windows opened at startup: `{ title, width, height, url }`. |
| `minimizeToTray` | bool | `false` | The close button hides the window to the tray instead of quitting. |

### 🚀 Backend & Sidecars
```json
"backend": {
  "command": "node",
  "args": ["server.js"],
  "files": ["server.js", "src/server/**/*.js"],
  "port": 4567
}
```
- **With `port` (Server mode):** The whole bundle extracts to `%TEMP%\brix_<name>`. The backend spawns there, and the app loads `http://127.0.0.1:<port>/`.
- **Without `port` (Bundled mode):** The backend runs as a hidden sidecar next to the app. The app loads its bundled assets over `brix://`.

### 🎨 Splash Screen & Tray
| Key | Type | Default | Description |
|---|---|---|---|
| `splash.enabled` | bool | `true` | Show a branded splash window while the app boots. |
| `splash.background` | string | gradient | CSS background of the splash window. |
| `splash.image` | string | — | Path to a PNG shown in the center of the splash window. |
| `tray` | bool\|object | `false` | System-tray icon. Object takes `{ icon, tooltip, menu: [...] }`. |

---

## 2. `installer/installer.json` — The Installer Config

Generated via `brixpack init installer`. Re-run `brixpack make` after editing.

### Installer Properties
| Key | Type | Description |
|---|---|---|
| `type` | string | `"zip"`, `"inno"`, `"nsis"`, or `"msi"`. |
| `appName` | string | Application name for shortcuts and registry. |
| `appVersion` | string | Version shown by the installer. |
| `outBase` | string | Output filename base (e.g. `my-app-setup`). |
| `outputDir` | string | Where the installer lands (e.g. `installer/dist`). |
| `files` | object[] | Everything installed. Defines `path`, `role`, and `subDir`. |

### Installer Types Output
- **zip:** Portable folder zip (`<outBase>-portable.zip`).
- **inno:** Inno Setup 6 (`<outBase>.exe`).
- **nsis:** NSIS 3 (`<outBase>.exe`).
- **msi:** WiX v3/v4 (`<outBase>.msi`).
