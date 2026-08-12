#!/usr/bin/env node
/**
 * Brix CLI - Main entry point
 * 
 * Architecture Overview:
 * ======================
 * Brix is a single-file Windows executable generator for web applications.
 * It bundles a web project (HTML/JS/CSS) into a compressed ZIP appended
 * to a pre-built Rust stub executable. The stub provides:
 * 
 * 1. WebView2-based window hosting (Windows system Chromium)
 * 2. Native bridge: window.brix.invoke(method, args) → Promise
 * 3. Extension sidecar IPC: unknown methods forwarded to external process
 * 4. Bundle integrity: SHA-256 footer verified at runtime
 * 5. Installer generation: zip, Inno Setup, NSIS, WiX (MSI)
 * 6. Dev mode: local HTTP server with live reload
 * 
 * Security Model:
 * ===============
 * - Runtime integrity: SHA-256 hash of bundle verified at startup
 * - Path sandboxing: all file operations restricted to data_dir/temp_dir
 * - IPC validation: method names validated, args size limited (64KB)
 * - Extension sidecar: opt-in, communicates via stdin/stdout JSON lines
 * - No eval: bridge only exposes predefined methods
 * - Path traversal prevention: resolve_scoped_path blocks ".." and absolute paths
 * 
 * Build Pipeline:
 * ===============
 * 1. brix init → creates .brix config (interactive or --yes)
 * 2. brix build → collects files → zip → appends .brix.json → signs with SHA-256 footer
 * 3. brix make zip|inno|nsis|msi → generates installer from installer/installer.json
 * 4. brix dev → starts local server (5174) + launches stub with --dev flag
 * 4. brix preview → builds if needed, launches exe directly
 * 
 * Extension Sidecar Protocol:
 * ===========================
 * - Stdin: receives JSON lines {id, method, args}
 * - Stdout: replies JSON lines {id, ok, result?, error?}
 * - One process per app session, communicates via stdio
 * - Unknown window.brix.invoke() methods forwarded automatically
 */

const { Command } = require('commander');
const fs = require('fs-extra');
const os = require('os');
const path = require('path');
const { spawn } = require('child_process');
const { glob } = require('glob');
const archiver = require('archiver');
const ResEdit = require('resedit');
const chalk = require('chalk');

const pkg = require('../package.json');

const { initInstaller, makeInstaller } = require('./installer');
const { initProject } = require('./init');
const program = new Command();

program
  .name('brix')
  .description('Brix: Convert web projects to desktop apps')
  .version(pkg.version);

/**
 * Normalizes a project-relative path to forward slashes.
 * Used throughout for cross-platform path consistency.
 */
function toForwardSlashes(p) {
  return String(p).replace(/\\/g, '/');
}

/**
 * True if a relative path escapes its base (contains ".." segments).
 * Security: prevents directory traversal in user-supplied paths.
 */
function isUnsafePath(p) {
  return toForwardSlashes(p).split('/').includes('..');
}

/**
 * Sanitizes a name into a safe Windows filename (original case preserved).
 * Removes invalid chars, collapses whitespace, preserves case.
 */
function sanitizeFileName(name) {
  return String(name)
    .replace(/[<>:"/\\|?*\x00-\x1F]/g, '')
    .replace(/\s+/g, '-')
    .replace(/-+/g, '-');
}

/**
 * Parses "x.y.z.w" into a 4-part version array (defaults 1.0.0.0).
 * Used for Windows resource version info (resedit).
 */
function parseVersion(str) {
  const parts = String(str || '')
    .split('.')
    .map((n) => parseInt(n, 10))
    .filter((n) => Number.isInteger(n) && n >= 0 && n <= 65535);
  while (parts.length < 4) parts.push(0);
  return parts;
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * Default paths excluded from the bundle. `.brix` is always handled separately
 * (the internal `.brix.json` is appended by the build instead), and Brix_Works
 * is where the output exe lands so previous builds never leak into the bundle.
 */
const DEFAULT_EXCLUDES = [
  'node_modules',
  '.git',
  'BRIX-APP',
  'Brix_Works',
  '.brix',
  '*.exe',
  '*.log',
  '*.WebView2',
  'temp_*.zip'
];

/**
 * Validates the .brix configuration
 */
async function validateConfig(config, configPath) {
    const root = path.dirname(configPath);
    if (!config.name) throw new Error('Missing "name" in .brix');
    if (typeof config.name !== 'string') throw new Error('"name" must be a string');
    if (config.name.length > 60) throw new Error('"name" must be 60 characters or fewer');
    if (!sanitizeFileName(config.name)) {
        throw new Error(`"name" contains only characters that are invalid in a filename: ${config.name}`);
    }
    if (/[^a-zA-Z0-9 _.-]/.test(config.name)) {
        console.warn(chalk.yellow(`   ⚠ App name "${config.name}" contains characters that are stripped from the exe filename (${sanitizeFileName(config.name)}.exe)`));
    }
    if (!config.entry) throw new Error('Missing "entry" in .brix');
    if (isUnsafePath(config.entry)) {
        throw new Error(`"entry" must stay inside the project folder: ${config.entry}`);
    }

    const entryPath = path.resolve(root, toForwardSlashes(config.entry));
    if (!(await fs.pathExists(entryPath))) {
        throw new Error(`Entry file not found: ${config.entry}`);
    }

    if (typeof config.version !== 'string' && config.version !== undefined) {
        throw new Error('"version" must be a string (e.g. "1.2.3")');
    }
    if (config.icon !== undefined) {
        if (typeof config.icon !== 'string' || !config.icon) throw new Error('"icon" must be a file path string');
        const iconPath = path.resolve(root, toForwardSlashes(config.icon));
        if (!(await fs.pathExists(iconPath))) {
            throw new Error(`Icon file not found: ${config.icon}`);
        }
        if (!/\.ico$/i.test(config.icon)) {
            console.warn(chalk.yellow(`   ⚠ Icon "${config.icon}" is not a .ico file; exe branding may fail`));
        }
    }

    for (const key of ['include', 'exclude']) {
        if (config[key] !== undefined) {
            if (!Array.isArray(config[key]) || !config[key].every((p) => typeof p === 'string')) {
                throw new Error(`"${key}" must be an array of path strings`);
            }
        }
    }

    if (config.backend) {
        if (config.backend.files !== undefined) {
            if (!Array.isArray(config.backend.files) || !config.backend.files.every((p) => typeof p === 'string')) {
                throw new Error('"backend.files" must be an array of path strings');
            }
        }
    }

    if (config.window) {
        if (config.window.width !== undefined && !(Number.isInteger(config.window.width) && config.window.width > 0)) {
            throw new Error('"window.width" must be a positive integer');
        }
        if (config.window.height !== undefined && !(Number.isInteger(config.window.height) && config.window.height > 0)) {
            throw new Error('"window.height" must be a positive integer');
        }
    }

    if (config.splash !== undefined) {
        if (typeof config.splash !== 'object' || Array.isArray(config.splash)) {
            throw new Error('"splash" must be an object');
        }
        if (config.splash.enabled !== undefined && typeof config.splash.enabled !== 'boolean') {
            throw new Error('"splash.enabled" must be a boolean');
        }
        if (config.splash.width !== undefined && !(Number.isInteger(config.splash.width) && config.splash.width > 0)) {
            throw new Error('"splash.width" must be a positive integer');
        }
        if (config.splash.height !== undefined && !(Number.isInteger(config.splash.height) && config.splash.height > 0)) {
            throw new Error('"splash.height" must be a positive integer');
        }
    }

    if (config.webview2 !== undefined) {
        if (typeof config.webview2 !== 'object' || Array.isArray(config.webview2)) {
            throw new Error('"webview2" must be an object');
        }
        if (config.webview2.fixedRuntimePath !== undefined &&
            (typeof config.webview2.fixedRuntimePath !== 'string' || !config.webview2.fixedRuntimePath)) {
            throw new Error('"webview2.fixedRuntimePath" must be a path string');
        }
    }

    if (config.tray !== undefined && typeof config.tray !== 'boolean') {
        if (typeof config.tray !== 'object' || Array.isArray(config.tray)) {
            throw new Error('"tray" must be a boolean or an object with { icon?, tooltip?, menu? }');
        }
        if (config.tray.icon !== undefined && (typeof config.tray.icon !== 'string' || !config.tray.icon)) {
            throw new Error('"tray.icon" must be a path string (png or ico)');
        }
        if (config.tray.tooltip !== undefined && typeof config.tray.tooltip !== 'string') {
            throw new Error('"tray.tooltip" must be a string');
        }
        if (config.tray.menu !== undefined) {
            if (!Array.isArray(config.tray.menu)) throw new Error('"tray.menu" must be an array');
            for (const item of config.tray.menu) {
                if (typeof item !== 'object' || item === null) {
                    throw new Error('every "tray.menu" entry must be an object');
                }
                if (typeof item.label !== 'string' && item.separator !== true) {
                    throw new Error('every non-separator "tray.menu" entry needs a "label" string');
                }
                if (item.id !== undefined && typeof item.id !== 'string') {
                    throw new Error('"tray.menu[].id" must be a string');
                }
                if (item.enabled !== undefined && typeof item.enabled !== 'boolean') {
                    throw new Error('"tray.menu[].enabled" must be a boolean');
                }
                if (item.checked !== undefined && typeof item.checked !== 'boolean') {
                    throw new Error('"tray.menu[].checked" must be a boolean');
                }
                if (item.separator !== undefined && typeof item.separator !== 'boolean') {
                    throw new Error('"tray.menu[].separator" must be a boolean');
                }
            }
        }
    }
    if (config.minimizeToTray !== undefined && typeof config.minimizeToTray !== 'boolean') {
        throw new Error('"minimizeToTray" must be a boolean');
    }
    if (config.devtools !== undefined && typeof config.devtools !== 'boolean') {
        throw new Error('"devtools" must be a boolean');
    }

    if (config.splash !== undefined) {
        if (config.splash.image !== undefined) {
            if (typeof config.splash.image !== 'string' || !config.splash.image) {
                throw new Error('"splash.image" must be a file path string (png)');
            }
            const imgPath = path.resolve(root, toForwardSlashes(config.splash.image));
            if (!(await fs.pathExists(imgPath))) {
                throw new Error(`Splash image not found: ${config.splash.image}`);
            }
        }
        if (config.splash.text !== undefined && typeof config.splash.text !== 'string') {
            throw new Error('"splash.text" must be a string');
        }
        if (config.splash.autoHide !== undefined && typeof config.splash.autoHide !== 'boolean') {
            throw new Error('"splash.autoHide" must be a boolean');
        }
    }

    if (config.update !== undefined) {
        if (typeof config.update !== 'object' || Array.isArray(config.update)) {
            throw new Error('"update" must be an object');
        }
        if (typeof config.update.url !== 'string' || !/^https:\/\//i.test(config.update.url)) {
            throw new Error('"update.url" must be an https:// URL pointing at the update manifest');
        }
        if (config.update.checkOnStart !== undefined && typeof config.update.checkOnStart !== 'boolean') {
            throw new Error('"update.checkOnStart" must be a boolean');
        }
        if (config.update.autoInstall !== undefined && typeof config.update.autoInstall !== 'boolean') {
            throw new Error('"update.autoInstall" must be a boolean');
        }
    }

    if (config.sign !== undefined) {
        if (typeof config.sign !== 'object' || Array.isArray(config.sign)) {
            throw new Error('"sign" must be an object');
        }
        if (config.sign.enabled !== undefined && typeof config.sign.enabled !== 'boolean') {
            throw new Error('"sign.enabled" must be a boolean');
        }
        if (config.sign.certFile !== undefined && (typeof config.sign.certFile !== 'string' || !config.sign.certFile)) {
            throw new Error('"sign.certFile" must be a path to a .pfx/.p12 certificate');
        }
        if (config.sign.certPassword !== undefined && typeof config.sign.certPassword !== 'string') {
            throw new Error('"sign.certPassword" must be a string');
        }
        if (config.sign.timestamp !== undefined && typeof config.sign.timestamp !== 'string') {
            throw new Error('"sign.timestamp" must be a timestamp URL string');
        }
        if (config.sign.signtool !== undefined && (typeof config.sign.signtool !== 'string' || !config.sign.signtool)) {
            throw new Error('"sign.signtool" must be a path/command string');
        }
        if (config.sign.command !== undefined &&
            (!Array.isArray(config.sign.command) || !config.sign.command.every((a) => typeof a === 'string'))) {
            throw new Error('"sign.command" must be an array of command arguments');
        }
    }

    if (config.plugins !== undefined) {
        if (!Array.isArray(config.plugins) || !config.plugins.every((p) => typeof p === 'string' && p)) {
            throw new Error('"plugins" must be an array of package names');
        }
    }

    if (config.windows !== undefined) {
        if (!Array.isArray(config.windows)) {
            throw new Error('"windows" must be an array of window objects');
        }
        for (const w of config.windows) {
            if (typeof w !== 'object' || w === null || Array.isArray(w)) {
                throw new Error('every "windows" entry must be an object');
            }
            if (w.title !== undefined && typeof w.title !== 'string') {
                throw new Error('"windows[].title" must be a string');
            }
            for (const dim of ['width', 'height']) {
                if (w[dim] !== undefined && (!Number.isInteger(w[dim]) || w[dim] < 1)) {
                    throw new Error(`"windows[].${dim}" must be a positive integer`);
                }
            }
            if (w.url !== undefined && (typeof w.url !== 'string' || !w.url)) {
                throw new Error('"windows[].url" must be a non-empty path string');
            }
        }
    }
}

/**
 * Loads and validates the .brix config.
 * `projectPath` may be a project folder or a .brix file; defaults to the
 * current directory. All relative paths inside the config are resolved
 * against the project root.
 */
async function loadConfig(projectPath) {
    let configPath;
    if (projectPath) {
        const resolved = path.resolve(projectPath);
        const stat = await fs.stat(resolved).catch(() => null);
        if (!stat) {
            throw new Error(`Project path not found: ${projectPath}`);
        }
        if (stat.isDirectory()) {
            configPath = path.join(resolved, '.brix');
            if (!(await fs.pathExists(configPath))) {
                throw new Error(`No .brix file found in ${projectPath}`);
            }
        } else {
            configPath = resolved;
        }
    } else {
        configPath = path.resolve(process.cwd(), '.brix');
        if (!(await fs.pathExists(configPath))) {
            throw new Error('.brix configuration file not found in the current directory');
        }
    }

    const config = await fs.readJson(configPath);
    await validateConfig(config, configPath);

    if (config.backend) {
        if (typeof config.backend.command !== 'string' || !config.backend.command) {
            throw new Error('"backend.command" must be a string (e.g. "node")');
        }
        if (config.backend.port !== undefined &&
            (!Number.isInteger(config.backend.port) || config.backend.port < 1 || config.backend.port > 65535)) {
            throw new Error('"backend.port" must be an integer between 1 and 65535');
        }
    }
    return { config, configPath, root: path.dirname(configPath) };
}

/**
 * Resolves the files that would be bundled, without copying anything.
 * Junctions and symlinks are dereferenced: the entry keeps its project-relative
 * name while the file content is read from its real path.
 * Returns [{ name, realPath, size }].
 */
async function collectFiles(config, configPath, root) {
    const includes = config.include || ['./**/*'];
    // User excludes are added on top of the built-in safety defaults —
    // a single "exclude" entry must never unbundle node_modules, .git, etc.
    const userExcludes = Array.isArray(config.exclude) ? config.exclude : [];
    const excludes = [...DEFAULT_EXCLUDES, ...userExcludes];

    // A magic-less ignore pattern matches only the exact path; also exclude
    // everything below it (e.g. "node_modules" -> "node_modules/**").
    // Basename-only patterns (no "/") are matched at every depth, so
    // "*.exe" also excludes nested binaries like out/my-app.exe.
    const hasMagic = (p) => /[*?[\]{}()!+@]/.test(p);
    const ignore = excludes.flatMap((p) => {
        const trimmed = String(p).replace(/\/+$/, '');
        if (trimmed.includes('/')) {
            return hasMagic(trimmed) ? [trimmed] : [trimmed, `${trimmed}/**`];
        }
        return hasMagic(trimmed)
            ? [trimmed, `**/${trimmed}`]
            : [trimmed, `${trimmed}/**`, `**/${trimmed}`, `**/${trimmed}/**`];
    });

    const matched = await glob(includes, {
        cwd: root,
        ignore,
        nodir: true,
        dot: true
    });

    const files = [];
    const seen = new Set();
    for (const file of matched) {
        const rel = toForwardSlashes(file).replace(/^\.\//, '');
        if (isUnsafePath(rel)) {
            console.warn(chalk.yellow(`   ⚠ Skipping path that escapes the project: ${file}`));
            continue;
        }
        // The user's .brix is never bundled; the internal .brix.json is appended instead.
        if (rel === '.brix') continue;

        const abs = path.join(root, file);
        let st;
        try {
            st = await fs.stat(abs); // follows junctions/symlinks
        } catch (e) {
            console.warn(chalk.yellow(`   ⚠ Skipping unreadable path: ${file}`));
            continue;
        }
        if (!st.isFile()) continue; // directory / reparse point that is not a file

        const real = await fs.realpath(abs);
        if (seen.has(real)) continue; // same underlying file matched twice
        seen.add(real);
        files.push({ name: rel, realPath: real, size: st.size });
    }
    return files;
}

/**
 * Resolves backend source files to bundle into _backend/.
 * Returns [{ name, realPath, size }].
 */
async function collectBackendFiles(config, root) {
    const out = [];
    if (!config.backend) return out;

    const add = async (file) => {
        const rel = toForwardSlashes(file);
        if (isUnsafePath(rel)) {
            console.warn(chalk.yellow(`   ⚠ Skipping backend path that escapes the project: ${file}`));
            return;
        }
        try {
            const abs = path.join(root, file);
            const st = await fs.stat(abs);
            if (!st.isFile()) return;
            out.push({ name: `_backend/${rel}`, realPath: await fs.realpath(abs), size: st.size });
        } catch (e) { /* skip unreadable */ }
    };

    if (config.backend.files) {
        for (const pattern of config.backend.files) {
            const matched = await glob(toForwardSlashes(pattern), { cwd: root, nodir: true });
            for (const file of matched) await add(file);
        }
    } else if (config.backend.args) {
        // Fallback: try to find the script in args
        for (const arg of config.backend.args) {
            const normArg = toForwardSlashes(arg);
            if (normArg.endsWith('.js') && !isUnsafePath(normArg) && await fs.pathExists(path.join(root, normArg))) {
                await add(normArg);
            }
        }
    }
    return out;
}

/**
 * Loads build-time plugins declared in "plugins": ["pkg-name", ...].
 * Plugins resolve from the project's node_modules first, then Brix's own.
 * Each plugin exports { name?, hooks: { preBundle?, transformFile?, postBuild? } }
 * or a plain object with the hook functions directly.
 */
async function loadPlugins(config, root) {
    if (!config.plugins || config.plugins.length === 0) return [];
    const loaded = [];
    for (const name of config.plugins) {
        const resolved = resolvePlugin(name, root);
        if (!resolved) {
            throw new Error(`Plugin "${name}" not found. Install it in the project (npm i ${name}) or in Brix.`);
        }
        let mod;
        try {
            mod = require(resolved);
        } catch (e) {
            throw new Error(`Plugin "${name}" failed to load: ${e.message}`);
        }
        const plugin = mod && mod.__esModule && mod.default ? mod.default : mod;
        if (typeof plugin !== 'object' && typeof plugin !== 'function') {
            throw new Error(`Plugin "${name}" must export an object or a factory function`);
        }
        if (typeof plugin === 'function') {
            loaded.push({ name, hooks: plugin() || {} });
        } else {
            loaded.push({ name, hooks: plugin.hooks || plugin });
        }
        console.log(chalk.gray(`   Loaded plugin: ${name}`));
    }
    return loaded;
}

function resolvePlugin(name, root) {
    // Relative/absolute paths resolve against the project root directly.
    if (name.startsWith('./') || name.startsWith('../') || path.isAbsolute(name)) {
        const p = path.resolve(root, toForwardSlashes(name));
        return fs.existsSync(p) ? p : null;
    }
    const candidates = [
        path.join(root, 'node_modules'),
        ...(require.resolve.paths ? require.resolve.paths(path.join(root, 'package.json')) || [] : []),
        path.join(__dirname, '..', 'node_modules'),
        path.join(__dirname, 'node_modules')
    ];
    for (const base of candidates) {
        try {
            const resolved = require.resolve(name, { paths: [base] });
            return resolved;
        } catch (e) { /* try next */ }
    }
    return null;
}

/**
 * Runs every plugin's `hookName` hook. Returns the last truthy result.
 * preBundle may return { files, backendFiles } to replace the file lists.
 */
async function runPluginHook(plugins, hookName, context) {
    let result = null;
    for (const plugin of plugins) {
        const fn = plugin.hooks[hookName];
        if (typeof fn !== 'function') continue;
        const r = await fn(context);
        if (r !== undefined && r !== null) result = r;
    }
    return result;
}

/**
 * Returns a per-file transformer when at least one plugin implements
 * transformFile({ name, content: Buffer }) -> Buffer | string | null.
 * Null means "keep the original file".
 */
function makeFileTransformer(plugins) {
    const fns = plugins
        .map((p) => p.hooks.transformFile)
        .filter((fn) => typeof fn === 'function');
    if (fns.length === 0) return null;
    return async function transformFile(name, content) {
        let out = content;
        for (const fn of fns) {
            const r = await fn({ name, content: out });
            if (r !== undefined && r !== null) {
                out = Buffer.isBuffer(r) ? r : Buffer.from(String(r));
            }
        }
        return out;
    };
}

/**
 * Signs the built exe with signtool. When "sign.certFile" is set, a standard
 * command is composed; a custom "sign.command" array overrides it entirely.
 */
async function signExe(sign, exePath, root) {
    const signtool = sign.signtool || 'signtool';
    let args;
    if (Array.isArray(sign.command)) {
        args = sign.command.map(String);
        if (!args.includes(exePath)) args.push(exePath);
    } else if (sign.certFile) {
        args = ['sign'];
        if (sign.algorithm) args.push('/fd', String(sign.algorithm));
        args.push('/f', path.resolve(root, toForwardSlashes(sign.certFile)));
        if (sign.certPassword) args.push('/p', String(sign.certPassword));
        if (sign.timestamp) args.push('/t', String(sign.timestamp));
        args.push(exePath);
    } else {
        return;
    }

    const { spawn } = require('child_process');
    const result = await new Promise((resolve) => {
        const child = spawn(signtool, args, { windowsHide: true });
        let out = '';
        child.stdout.on('data', (d) => (out += d));
        child.stderr.on('data', (d) => (out += d));
        child.on('close', (code) => resolve({ code, out }));
    });

    if (result.code === 0) {
        console.log(chalk.gray('   ✔ Signed with signtool.'));
    } else {
        console.warn(chalk.yellow(`   ⚠ Signing failed (exit ${result.code}): ${result.out.trim()}`));
        console.warn(chalk.yellow('     The exe is built; install the Windows SDK signtool and re-run.'));
    }
}

program
  .command('build')
  .description('Bundle a web project into a standalone Windows executable')
  .argument('[project]', 'project folder or .brix file (default: current directory)')
  .option('--out <dir>', 'output directory for the executable (default: <project>/Brix_Works)')
  .option('--list', 'print the files that would be bundled and exit without building')
  .action(async (project, options) => {
    const zipPath = path.join(os.tmpdir(), `brix-build-${process.pid}-${Date.now()}.zip`);
    try {
      const { config, configPath, root } = await loadConfig(project);
      const appName = config.name || 'BrixApp';
      const outDir = options.out ? path.resolve(options.out) : path.join(root, 'Brix_Works');
      const outputPathFinal = path.join(outDir, `${sanitizeFileName(appName)}.exe`);

      console.log(chalk.cyan(`\n🚀 Building ${chalk.bold(appName)}...`));

      // 0. Collect the files to bundle (no staging folder, no project pollution)
      const files = await collectFiles(config, configPath, root);
      const backendFiles = await collectBackendFiles(config, root);

      if (options.list) {
        const total = files.concat(backendFiles).reduce((n, f) => n + f.size, 0);
        console.log(chalk.gray(`\n📦 ${files.length + backendFiles.length} files (${formatBytes(total)}) would be bundled:`));
        for (const f of files) console.log(chalk.gray(`   ${f.name}`));
        for (const f of backendFiles) console.log(chalk.gray(`   ${f.name}`));
        console.log(chalk.gray(`   → ${chalk.bold(outputPathFinal)}`));
        return;
      }

      // 1. Prepare Metadata
      const internalConfig = {
          name: config.name,
          entry: toForwardSlashes(config.entry).replace(/^(\.\/|\/)/, ''),
          window: config.window || { width: 1000, height: 700 },
          backend: config.backend || undefined,
          tray: config.tray === undefined ? undefined : config.tray,
          minimizeToTray: config.minimizeToTray === true,
          devtools: config.devtools === true,
          splash: config.splash === undefined ? undefined : config.splash,
          webview2: config.webview2 === undefined ? undefined : config.webview2,
          update: config.update === undefined ? undefined : config.update,
          windows: config.windows === undefined ? undefined : config.windows
      };

      // Load build-time plugins (preBundle / transformFile / postBuild hooks)
      const plugins = await loadPlugins(config, root);

      // 1b. Let plugins adjust the file list before anything is compressed.
      const filesBefore = { files, backendFiles };
      const pluginFiles = await runPluginHook(plugins, 'preBundle', { config, files: filesBefore.files, backendFiles: filesBefore.backendFiles });
      if (pluginFiles) {
          if (Array.isArray(pluginFiles.files)) files.splice(0, files.length, ...pluginFiles.files);
          if (Array.isArray(pluginFiles.backendFiles)) backendFiles.splice(0, backendFiles.length, ...pluginFiles.backendFiles);
      }

      // 2. Create Zip Bundle
      console.log(chalk.gray('   Compressing assets...'));
      const output = fs.createWriteStream(zipPath);
      const archive = archiver('zip', { zlib: { level: 9 } });

      archive.pipe(output);
      const transformFile = makeFileTransformer(plugins);
      for (const f of files) {
          if (transformFile) {
              // A transform hook exists: read the file so plugins can rewrite it.
              const transformed = await transformFile(f.name, await fs.readFile(f.realPath));
              if (transformed) {
                  archive.append(transformed, { name: f.name });
                  continue;
              }
          }
          archive.file(f.realPath, { name: f.name });
      }
      archive.append(JSON.stringify(internalConfig), { name: '.brix.json' });

      for (const f of backendFiles) {
        console.log(chalk.gray(`   Bundling backend file: ${f.name.replace(/^_backend\//, '')}`));
        archive.file(f.realPath, { name: f.name });
      }

      // Include the icon in the bundle for the native host to use as window icon
      let bundleIconPath = config.icon ? path.resolve(root, config.icon) : path.resolve(__dirname, '../icon.ico');
      if (await fs.pathExists(bundleIconPath)) {
          archive.file(await fs.realpath(bundleIconPath), { name: '_app_icon.ico' });
      }

      // Optional splash image (png) and custom tray icon (png/ico)
      if (config.splash && config.splash.image) {
          const imgPath = path.resolve(root, toForwardSlashes(config.splash.image));
          if (await fs.pathExists(imgPath)) {
              archive.file(await fs.realpath(imgPath), { name: '_splash.png' });
          }
      }
      if (config.tray && typeof config.tray === 'object' && config.tray.icon) {
          const trayPath = path.resolve(root, toForwardSlashes(config.tray.icon));
          if (await fs.pathExists(trayPath)) {
              const ext = path.extname(trayPath).toLowerCase();
              const bundleName = ext === '.ico' ? '_tray_icon.ico' : '_tray_icon.png';
              archive.file(await fs.realpath(trayPath), { name: bundleName });
          }
      }

      await archive.finalize();
      await new Promise((resolve, reject) => {
          output.on('close', resolve);
          output.on('error', reject);
      });

      const zipSize = (await fs.stat(zipPath)).size;
      if (zipSize > 50 * 1024 * 1024) {
          console.warn(chalk.yellow(`   ⚠ Bundle is ${formatBytes(zipSize)} — large bundles bloat the exe and slow startup. Review "include" / "exclude" patterns.`));
      }

      // 3. Prepare Binary
      const possibleStubPaths = [
        path.resolve(__dirname, '../bin/stub.exe'),
        path.resolve(__dirname, 'stub/target/release/brix-stub.exe'),
        path.resolve(__dirname, '../src/stub/target/release/brix-stub.exe'),
        path.resolve(__dirname, 'bin/stub.exe')
      ];

      let stubPath = possibleStubPaths.find(p => fs.existsSync(p));
      if (!stubPath) throw new Error('Native host engine (stub.exe) not found. Reinstall Brix.');

      const stubData = await fs.readFile(stubPath);

      // 4. Branding
      let brandedBinary = stubData;
      try {
          const exe = ResEdit.NtExecutable.from(stubData);
          const res = ResEdit.NtExecutableResource.from(exe);

          // Apply Icon
          let iconPath = config.icon ? path.resolve(root, config.icon) : path.resolve(__dirname, '../icon.ico');
          if (fs.existsSync(iconPath)) {
              const iconFile = ResEdit.Data.IconFile.from(fs.readFileSync(iconPath));
              ResEdit.Resource.IconGroupEntry.replaceIconsForResource(
                  res.entries,
                  1,
                  1033,
                  iconFile.icons.map(icon => icon.data)
              );
          }

          // Apply Version Info Metadata (Optional but professional)
          try {
              const version = parseVersion(config.version);
              const vi = new ResEdit.Resource.VersionInfo();
              vi.setFileVersion(version[0], version[1], version[2], version[3], 1033);
              vi.setProductVersion(version[0], version[1], version[2], version[3], 1033);
              vi.setStringValues({ lang: 1033, codepage: 1200 }, {
                  CompanyName: 'HadesWorld',
                  FileDescription: config.name,
                  LegalCopyright: `© ${new Date().getFullYear()} Copyright HaadiAli, HadesWorld`,
                  ProductName: config.name,
              });
              vi.outputToResourceEntries(res.entries);
          } catch (e) { /* ignore version info errors */ }

          res.outputResource(exe);
          brandedBinary = Buffer.from(exe.generate());
          console.log(chalk.gray('   ✔ Metadata and branding applied.'));
      } catch (brandingErr) {
          console.warn(chalk.yellow(`   ⚠ Branding failed: ${brandingErr.message}. Proceeding without metadata.`));
      }

      // 5. Stitch ZIP and Footer (streamed: never load the whole bundle into Node's heap)
      // Footer layout (little-endian): [u64 zip size][32 bytes SHA-256 of the zip].
      // The runtime verifies the hash before serving, so a tampered or
      // truncated bundle is refused instead of shipping a broken app.
      const { createHash } = require('crypto');
      const zipBuf = await fs.readFile(zipPath);
      const zipHash = createHash('sha256').update(zipBuf).digest();
      const footer = Buffer.alloc(8 + 32);
      footer.writeBigUInt64LE(BigInt(zipSize));
      zipHash.copy(footer, 8);

      await fs.ensureDir(outDir);
      await fs.writeFile(outputPathFinal, brandedBinary);
      await new Promise((resolve, reject) => {
        const inp = fs.createReadStream(zipPath);
        const out = fs.createWriteStream(outputPathFinal, { flags: 'a' });
        inp.on('error', reject);
        out.on('error', reject);
        out.on('close', resolve);
        inp.pipe(out);
      });
      await fs.appendFile(outputPathFinal, footer);

      // 6. Code signing (signtool) — after the exe is complete.
      if (config.sign && config.sign.enabled !== false) {
          await signExe(config.sign, outputPathFinal, root);
      }

      // 7. postBuild plugin hook (e.g. verify, publish, checksums).
      await runPluginHook(plugins, 'postBuild', { config, exePath: outputPathFinal });

      console.log(chalk.green(`\n✔ Build complete! ✨`));
      console.log(chalk.white(`   Location: ${chalk.bold(outputPathFinal)}\n`));

    } catch (err) {
      console.error(chalk.red('\n✖ Build failed:'), err.message);
      process.exitCode = 1;
    } finally {
      // Never leave the intermediate zip behind, even on failure
      if (fs.existsSync(zipPath)) {
        try { await fs.remove(zipPath); } catch (e) { /* ignore */ }
      }
    }
  });

program
  .command('init')
  .description('Initialize a .brix config or app resources (installer, etc.)')
  .argument('[project]', 'project folder (default: current directory)')
  .option('--force', 'overwrite an existing .brix file')
  .action(async (project, options) => {
    try {
      await initProject(project, options);
    } catch (err) {
      console.error(chalk.red('\n✖ Init failed:'), err.message);
      process.exitCode = 1;
    }
  })
  .command('installer')
  .description('Create an installer configuration for a built Brix app (interactive wizard)')
  .argument('[project]', 'project folder or .brix file (default: current directory)')
  .option('--type <type>', 'installer type: zip | inno | nsis | msi (skip the picker)')
  .option('--files <list>', 'comma-separated files to include (skip the file picker)')
  .option('--name <name>', 'application name')
  .option('--appVersion <version>', 'application version')
  .option('--outBase <base>', 'output filename base (e.g. myapp-setup)')
  .action(async (project, options) => {
    try {
      await initInstaller(project, options);
    } catch (err) {
      console.error(chalk.red('\n✖ Init installer failed:'), err.message);
      process.exitCode = 1;
    }
  });

program
  .command('make')
  .description('Build an installer: zip (portable) | inno | nsis | msi')
  .argument('[target]', 'zip | inno | nsis | msi (default: type from installer.json)')
  .option('--project <dir>', 'project folder containing installer/installer.json (default: current directory)')
  .option('--out <dir>', 'output directory for the installer')
  .option('--name <name>', 'override application name')
  .option('--appVersion <version>', 'override application version')
  .action(async (target, options) => {
    try {
      await makeInstaller(target, options);
    } catch (err) {
      console.error(chalk.red('\n✖ Make failed:'), err.message);
      process.exitCode = 1;
    }
  });

/* ------------------------------------------------------------------ */
/*  Dev mode: serve a project over a local server and launch it in     */
/*  the Brix host with --dev (live reload by refreshing the window).   */
/* ------------------------------------------------------------------ */

/* ------------------------------------------------------------------ */
/*  Dev mode: serve a project over a local server and launch it in     */
/*  the Brix host with --dev (live reload by refreshing the window).   */
/* ------------------------------------------------------------------ */

/**
 * DEV_BRIDGE - Injected into served HTML during `brix dev`
 * 
 * This bridge mirrors the Rust BRIDGE_JS in src/stub/src/main.rs.
 * Both must be kept in sync for consistent behavior between
 * dev mode (served HTML) and production (bundled app).
 * 
 * Security Model:
 * ===============
 * - Only exposes predefined methods: invoke, on, off, _handle, _handleEvent, _handleExtension, hmr
 * - No eval() or arbitrary code execution
 * - Promise-based invoke with sequential IDs prevents replay attacks
 * - Extension sidecar: unknown methods forwarded to native sidecar via IPC
 * - HMR stub: no-op placeholders for Vite HMR integration
 * 
 * IPC Protocol:
 * =============
 * window.brix.invoke(method, args) → Promise
 *   - Generates sequential ID
 *   - Registers promise in _pending[id]
 *   - Posts {id, method, args} to native host via _post()
 *   - Native responds via _handle(json) or _handleExtension(json)
 *   - Resolves/rejects original promise
 * 
 * Extension Sidecar Protocol (for unknown methods):
 * =================================================
 * - Native forwards unknown methods to extension sidecar via stdin
 * - Sidecar reads JSON lines: {id, method, args}
 * - Sidecar writes JSON lines: {id, ok, result?, error?}
 * - Response routed back via _handleExtension → _handle → resolves promise
 * 
 * HMR Integration:
 * ================
 * - hmr.accept/reject: no-op stubs for Vite HMR client
 * - When Vite HMR is integrated, these will be called on module updates
 * - Current implementation: refresh on file change (brix dev)
 */
const DEV_BRIDGE = `(function () {
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
})();`;

const DEV_MIME = {
  '.html': 'text/html', '.htm': 'text/html', '.css': 'text/css',
  '.js': 'application/javascript', '.mjs': 'text/javascript', '.cjs': 'text/javascript',
  '.json': 'application/json', '.map': 'application/json', '.webmanifest': 'application/manifest+json',
  '.wasm': 'application/wasm', '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg',
  '.gif': 'image/gif', '.svg': 'image/svg+xml', '.ico': 'image/x-icon', '.webp': 'image/webp',
  '.avif': 'image/avif', '.bmp': 'image/bmp', '.woff2': 'font/woff2', '.woff': 'font/woff',
  '.ttf': 'font/ttf', '.otf': 'font/otf', '.eot': 'application/vnd.ms-fontobject',
  '.mp3': 'audio/mpeg', '.wav': 'audio/wav', '.ogg': 'audio/ogg', '.m4a': 'audio/mp4',
  '.flac': 'audio/flac', '.mp4': 'video/mp4', '.webm': 'video/webm', '.ogv': 'video/ogg',
  '.pdf': 'application/pdf', '.txt': 'text/plain', '.md': 'text/markdown', '.csv': 'text/csv',
  '.xml': 'application/xml'
};

function findDevExe(root) {
  const outDir = path.join(root, 'Brix_Works');
  if (!fs.existsSync(outDir)) return null;
  const exes = fs.readdirSync(outDir).filter((f) => f.toLowerCase().endsWith('.exe'));
  return exes.length ? path.join(outDir, exes[0]) : null;
}

function startDevServer(root, port, entryHtml) {
  const http = require('http');
  const rootResolved = path.resolve(root);
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      let urlPath = decodeURIComponent((req.url || '/').split('?')[0]);
      if (urlPath === '/') urlPath = '/' + (entryHtml || 'index.html');
      const filePath = path.join(rootResolved, urlPath);
      if (!filePath.startsWith(rootResolved)) {
        res.writeHead(403); res.end('Forbidden'); return;
      }
      fs.readFile(filePath, (err, data) => {
        if (err) {
          // SPA fallback to the entry document.
          const fallback = path.join(rootResolved, entryHtml || 'index.html');
          fs.readFile(fallback, (e2, d2) => {
            if (e2) { res.writeHead(404); res.end('Not found'); return; }
            serveHtml(res, d2.toString());
          });
          return;
        }
        const ext = path.extname(filePath).toLowerCase();
        if (ext === '.html' || ext === '.htm') {
          serveHtml(res, data.toString());
          return;
        }
        res.writeHead(200, { 'Content-Type': DEV_MIME[ext] || 'application/octet-stream' });
        res.end(data);
      });
    });
    server.on('error', reject);
    server.listen(port, '127.0.0.1', () => resolve(server));
  });

  function serveHtml(res, html) {
    let out = html;
    if (/<head[^>]*>/i.test(out)) {
      out = out.replace(/<head[^>]*>/i, (m) => `${m}<script>${DEV_BRIDGE}</script>`);
    } else {
      out = `<script>${DEV_BRIDGE}</script>` + out;
    }
    res.writeHead(200, { 'Content-Type': 'text/html' });
    res.end(out);
  }
}

program
  .command('dev')
  .description('Serve a project over a local dev server and launch it in the Brix host (live reload via refresh)')
  .argument('[project]', 'project folder (default: current directory)')
  .option('--port <port>', 'dev server port', '5174')
  .option('--host <host>', 'dev server host', '127.0.0.1')
  .action(async (project, options) => {
    const { spawn } = require('child_process');
    try {
      const root = path.resolve(project || process.cwd());
      if (!fs.existsSync(root)) throw new Error(`Not found: ${root}`);
      const { config } = await loadConfig(root).catch(() => ({ config: null }));
      const entryHtml = config && config.entry ? path.basename(config.entry) : 'index.html';

      // Dev reuses the built exe (it already carries a valid footer); build
      // first if the project has never been built.
      let exe = findDevExe(root);
      if (!exe) {
        console.log(chalk.gray('   No built exe yet — running a one-time build...'));
        await new Promise((resolve, reject) => {
          const child = spawn(process.execPath, [__filename, 'build', root], { stdio: 'inherit', windowsHide: true });
          child.on('exit', (code) => (code === 0 ? resolve() : reject(new Error('build failed'))));
        });
        exe = findDevExe(root);
      }
      if (!exe) throw new Error('Could not find or build a Brix exe.');

      const port = parseInt(options.port, 10) || 5174;
      const server = await startDevServer(root, port, entryHtml);
      const url = `http://${options.host}:${port}/`;
      console.log(chalk.cyan(`\n🔧 Dev server: ${url}`));
      console.log(chalk.gray('   Edit files and refresh the app window. Ctrl+C to stop.\n'));

      const child = spawn(exe, ['--dev', url, '--title', (config && config.name) || path.basename(root)], {
        windowsHide: true,
        stdio: 'ignore'
      });
      const shutdown = () => {
        try { child.kill(); } catch (e) { /* ignore */ }
        try { server.close(); } catch (e) { /* ignore */ }
        process.exit(0);
      };
      child.on('exit', () => shutdown());
      process.on('SIGINT', shutdown);
      process.on('SIGTERM', shutdown);
    } catch (err) {
      console.error(chalk.red('\n✖ Dev failed:'), err.message);
      process.exitCode = 1;
    }
  });

// ── brix hash ──────────────────────────────────────────────────────
program
  .command('hash')
  .description('Verify the SHA-256 integrity footer of a Brix executable')
  .argument('[exe]', 'path to .brix.exe (default: built exe in Brix_Works)')
  .action(async exePath => {
    let exe = exePath || (async () => {
      const root = process.cwd();
      // try common locations
      for (const p of [path.join(root, 'Brix_Works', 'stub.exe'), path.join(root, 'bin', 'stub.exe')]) {
        if (fs.existsSync(p)) return p;
      }
      // fallback: search up parents
      let dir = root;
      while (dir) {
        const cand = path.join(dir, 'stub.exe');
        if (fs.existsSync(cand)) return cand;
        dir = path.dirname(dir);
      }
      return null;
    })();
    exe = await exe;
    if (!exe) throw new Error('No Brix exe found; run `brix build` first.');
    const buf = fs.readFileSync(exe);
    const fileSize = buf.length;
    const FOOTER_LEN = 40;
    if (fileSize < FOOTER_LEN) throw new Error('Exe too small to have a footer.');
    const footerBuf = buf.subarray(fileSize - FOOTER_LEN);
    const zipSize = Number(footerBuf.subarray(0, 8).readBigUInt64LE());
    const expectedHash = footerBuf.subarray(8, 40);
    if (zipSize === 0 || zipSize > fileSize - FOOTER_LEN) {
      throw new Error('Invalid footer zip size.');
    }
    const bundleStart = fileSize - FOOTER_LEN - zipSize;
    const bundleEnd = fileSize - FOOTER_LEN;
    const bundle = buf.subarray(bundleStart, bundleEnd);
    const { createHash } = require('crypto');
    const actualHash = createHash('sha256').update(bundle).digest();
    const ok = actualHash.equals(expectedHash);
    console.log(ok
      ? chalk.green(`✔ Integrity OK — hash matches (zip size: ${zipSize} bytes)`)
      : chalk.red(`✖ Integrity FAILED — hash mismatch. Expected: ${expectedHash.toString('hex')}, Actual: ${actualHash.toString('hex')}`));
  });

// ── brix preview ────────────────────────────────────────────────────
program
  .command('preview')
  .description('Build (if needed) and launch the Brix app (like `brix build` then run the exe)')
  .argument('[project]', 'project folder (default: current directory)')
  .option('--port <port>', 'dev server port (ignored for preview)', '5174')
  .option('--host <host>', 'dev server host (ignored)', '127.0.0.1')
  .action(async (project, options) => {
    const root = path.resolve(project || process.cwd());
    if (!fs.existsSync(root)) throw new Error(`Not found: ${root}`);
    // build if needed
    console.log(chalk.gray('   Ensuring built exe exists...'));
    await new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [__filename, 'build', root], { stdio: 'inherit', windowsHide: true });
      child.on('exit', (code) => (code === 0 ? resolve() : reject(new Error('build failed'))));
    });
    const exe = findDevExe(root);
    if (!exe) throw new Error('Could not find built exe after build.');
    console.log(chalk.cyan(`\n▶ Launching app from ${exe}...\n`));
    const child = spawn(exe, [], { windowsHide: true });
    const shutdown = () => {
      try { child.kill(); } catch (e) { /* ignore */ }
      process.exit(0);
    };
    child.on('exit', () => shutdown());
    process.on('SIGINT', shutdown);
    process.on('SIGTERM', shutdown);
  });

program.parse(process.argv);
