/**
 * `brix init <project>` — scans a project folder and writes a `.brix`
 * config that lists every folder and root file, with entry point, icon and
 * backend detected where possible. The user then edits `.brix` freely and
 * runs `brix build`.
 */
const fs = require('fs-extra');
const path = require('path');
const chalk = require('chalk');

const IGNORED_DIRS = new Set(['node_modules', '.git', 'Brix_Works', 'installer', 'BRIX-APP', 'dist-webview']);
const IGNORED_ROOT_FILES = new Set(['.brix', '.brix.json']);
const ENTRY_CANDIDATES = [
    'index.html',
    'dist/index.html',
    'public/index.html',
    'src/index.html',
    'build/index.html',
    'docs/index.html',
    'app/index.html'
];
const ICON_CANDIDATES = [
    'icon.ico', 'favicon.ico', 'logo.ico', 'app.ico',
    'public/favicon.ico', 'src/favicon.ico', 'static/favicon.ico'
];
const BACKEND_CANDIDATES = [
    'server.js', 'app.js', 'index.js', 'main.js',
    'server.mjs', 'app.mjs', 'index.mjs', 'main.mjs',
    'server.py', 'app.py', 'main.py'
];

function backendCommandFor(file) {
    return file.endsWith('.py') ? 'python' : 'node';
}

async function listTopLevel(root) {
    const dirs = [];
    const files = [];
    const entries = await fs.readdir(root, { withFileTypes: true }).catch(() => []);
    for (const e of entries) {
        if (e.name.startsWith('.') && e.name !== '.brix') continue;
        if (e.isDirectory() && !IGNORED_DIRS.has(e.name)) dirs.push(e.name);
        else if (e.isFile() && !IGNORED_ROOT_FILES.has(e.name)) files.push(e.name);
    }
    dirs.sort((a, b) => a.localeCompare(b));
    files.sort((a, b) => a.localeCompare(b));
    return { dirs, files };
}

async function detectEntry(root, files, dirs) {
    for (const c of ENTRY_CANDIDATES) {
        if (files.includes(c)) return c;
        if (c.includes('/')) {
            const [dir, file] = c.split('/');
            if (dirs.includes(dir) && (await fs.pathExists(path.join(root, dir, file)))) return c;
        }
    }
    const rootHtml = files.filter((f) => f.endsWith('.html'));
    if (rootHtml.length === 1) return rootHtml[0];
    return null;
}

async function detectIcon(root, files, dirs) {
    for (const c of ICON_CANDIDATES) {
        const p = path.join(root, c);
        if (await fs.pathExists(p)) return c;
    }
    const rootIco = files.find((f) => f.endsWith('.ico'));
    return rootIco || null;
}

function detectBackend(root, files) {
    for (const c of BACKEND_CANDIDATES) {
        if (files.includes(c)) return c;
    }
    return null;
}

async function initProject(project, options) {
    let root = project ? path.resolve(project) : process.cwd();
    const stat = await fs.stat(root).catch(() => null);
    if (!stat) throw new Error(`Not found: ${root}`);
    if (stat.isFile()) root = path.dirname(root);
    if (!(await fs.stat(root).catch(() => null))?.isDirectory()) {
        throw new Error(`Not a directory: ${root}`);
    }

    const configPath = path.join(root, '.brix');
    if ((await fs.pathExists(configPath)) && !options.force) {
        throw new Error(`.brix already exists in ${root} — use --force to overwrite it`);
    }

    const { dirs, files } = await listTopLevel(root);

    const entry = await detectEntry(root, files, dirs);
    if (!entry) {
        console.log(chalk.yellow('   ⚠ No index.html found — set "entry" yourself (any HTML file or dist/index.html).'));
    }

    const icon = await detectIcon(root, files, dirs);
    if (icon && !icon.endsWith('.ico')) {
        console.log(chalk.yellow(`   ⚠ "${icon}" is not an .ico — the exe icon needs an ICO file. Point "icon" at any .ico, or omit it to use Brix's default.`));
    }

    const backendFile = detectBackend(root, files);
    if (backendFile) {
        console.log(chalk.cyan(`   Detected backend: ${backendFile} (bundle it as a hidden sidecar, or set a port for server mode).`));
    }

    // "include" lists every folder and root file so the user can edit it.
    const include = [
        ...dirs.map((d) => `${d}/**/*`),
        ...files.filter((f) => f !== entry)
    ];
    if (entry) include.unshift(entry);

    // Listed for clarity — the build always applies these on top anyway.
    const exclude = ['node_modules', '.git', 'Brix_Works', 'BRIX-APP', '*.exe', '*.log', '*.WebView2', 'temp_*.zip'];

    const config = {
        name: path.basename(root),
        version: '0.1.0',
        entry: entry || 'index.html',
        window: { width: 1000, height: 700 },
        include,
        exclude
    };
    if (icon) config.icon = icon;
    if (backendFile) {
        config.backend = {
            command: backendCommandFor(backendFile),
            args: [backendFile],
            files: [backendFile]
        };
    }

    await fs.writeJson(configPath, config, { spaces: 2 });
    console.log(chalk.green(`\n✔ Wrote .brix with ${dirs.length} folders and ${files.length} root files listed.`));
    console.log(chalk.gray(`   Edit ${path.join(root, '.brix')} to tweak anything, then run: brix build ${project || ''}`));
    if (backendFile) {
        console.log(chalk.gray(`   backend is a ${config.backend.command} sidecar — add a "port" to switch to server mode (see syntax.md).`));
    }
    return configPath;
}

module.exports = { initProject };
