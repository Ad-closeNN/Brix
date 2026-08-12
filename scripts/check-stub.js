#!/usr/bin/env node
// Publish guard: bin/stub.exe must exist and be newer than the stub sources.
// Rebuild it with:
//   cd src/stub && cargo build --release
//   copy target\release\brix-stub.exe ..\..\bin\stub.exe
'use strict';

const fs = require('fs');
const path = require('path');

const root = path.resolve(__dirname, '..');
const stubExe = path.join(root, 'bin', 'stub.exe');
const sources = ['src/stub/src/main.rs', 'src/stub/Cargo.toml'];

if (!fs.existsSync(stubExe)) {
  console.error('✖ bin/stub.exe is missing. Build it before publishing:');
  console.error('    cd src/stub && cargo build --release');
  console.error('    copy target\\release\\brix-stub.exe ..\\..\\bin\\stub.exe');
  process.exit(1);
}

const exeMtime = fs.statSync(stubExe).mtimeMs;
const stale = sources.filter((f) => fs.statSync(path.join(root, f)).mtimeMs > exeMtime);
if (stale.length) {
  console.error(`✖ bin/stub.exe is older than ${stale.join(', ')}. Rebuild and recopy the stub before publishing:`);
  console.error('    cd src/stub && cargo build --release');
  console.error('    copy target\\release\\brix-stub.exe ..\\..\\bin\\stub.exe');
  process.exit(1);
}

console.log('✔ bin/stub.exe is up to date.');
