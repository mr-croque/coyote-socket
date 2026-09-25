#!/usr/bin/env node

/**
 * Build the Windows release and copy it to a timestamped folder.
 *
 * Run this from Windows PowerShell, not from WSL. The Windows build is the
 * one that can use the Windows Bluetooth stack.
 *
 * Usage:
 *   npm run tauri:build:snapshot
 *   npm run tauri:build:snapshot -- --dir D:\some\other\folder
 *
 * Default destination:
 *   D:\workarea\2026\temp\coyote-socket-builds\CoyoteSocket-<version>-<timestamp>\coyote-socket.exe
 */

import fs from 'fs';
import { spawn } from 'child_process';
import path from 'path';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const ROOT_DIR = path.join(__dirname, '..');

const CARGO_TOML_PATH = path.join(ROOT_DIR, 'src-tauri', 'Cargo.toml');
const RELEASE_EXE = path.join(ROOT_DIR, 'src-tauri', 'target', 'release', 'coyote-socket.exe');
const DEFAULT_DEST = 'D:\\workarea\\2026\\temp\\coyote-socket-builds';

function destinationDir() {
    const flagIndex = process.argv.indexOf('--dir');
    if (flagIndex !== -1 && process.argv[flagIndex + 1]) {
        return process.argv[flagIndex + 1];
    }
    return DEFAULT_DEST;
}

function getVersion() {
    const content = fs.readFileSync(CARGO_TOML_PATH, 'utf8');
    const match = content.match(/^version\s*=\s*"([\d.]+)"/m);
    if (!match) {
        throw new Error('Could not find version in Cargo.toml');
    }
    return match[1];
}

function timestamp() {
    const now = new Date();
    const pad = (value) => String(value).padStart(2, '0');
    return [
        now.getFullYear(),
        pad(now.getMonth() + 1),
        pad(now.getDate()),
    ].join('-') + '_' + [
        pad(now.getHours()),
        pad(now.getMinutes()),
        pad(now.getSeconds()),
    ].join('-');
}

function runTauriBuild() {
    return new Promise((resolve, reject) => {
        console.log('Starting Windows release build...\n');

        const build = spawn('npm.cmd', ['run', 'tauri', 'build'], {
            cwd: ROOT_DIR,
            stdio: 'inherit',
            shell: true,
        });

        build.on('close', (code) => {
            if (code === 0) {
                resolve();
            } else {
                reject(new Error(`Build failed with exit code ${code}`));
            }
        });

        build.on('error', (error) => {
            reject(error);
        });
    });
}

async function main() {
    if (process.platform !== 'win32') {
        console.error('This script must be run from Windows PowerShell, not from WSL.');
        console.error('A build made inside WSL cannot use the Windows Bluetooth radio.');
        process.exit(1);
    }

    const version = getVersion();
    const folderName = `CoyoteSocket-${version}-${timestamp()}`;
    const destRoot = destinationDir();
    const destDir = path.join(destRoot, folderName);
    const destExe = path.join(destDir, 'coyote-socket.exe');

    await runTauriBuild();

    if (!fs.existsSync(RELEASE_EXE)) {
        throw new Error(`Build finished but ${RELEASE_EXE} was not found`);
    }

    fs.mkdirSync(destDir, { recursive: true });
    fs.copyFileSync(RELEASE_EXE, destExe);

    console.log(`\nCopied build to ${destExe}`);
}

main().catch((error) => {
    console.error('\nSnapshot build failed:', error.message);
    process.exit(1);
});
