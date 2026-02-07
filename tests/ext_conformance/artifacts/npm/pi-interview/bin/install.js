#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const os = require("os");

const EXTENSION_NAME = "interview";
const TARGET_DIR = path.join(os.homedir(), ".pi", "agent", "extensions", EXTENSION_NAME);
const SOURCE_DIR = path.join(__dirname, "..");

const FILES_TO_COPY = [
  "index.ts",
  "schema.ts", 
  "server.ts",
  "settings.ts",
  "package.json",
];

const DIRS_TO_COPY = ["form"];

function ensureDir(dir) {
  if (!fs.existsSync(dir)) {
    fs.mkdirSync(dir, { recursive: true });
  }
}

function copyFile(src, dest) {
  fs.copyFileSync(src, dest);
}

function copyDir(src, dest) {
  ensureDir(dest);
  const entries = fs.readdirSync(src, { withFileTypes: true });
  for (const entry of entries) {
    const srcPath = path.join(src, entry.name);
    const destPath = path.join(dest, entry.name);
    if (entry.isDirectory()) {
      copyDir(srcPath, destPath);
    } else {
      copyFile(srcPath, destPath);
    }
  }
}

function getVersion() {
  try {
    const pkg = JSON.parse(fs.readFileSync(path.join(SOURCE_DIR, "package.json"), "utf-8"));
    return pkg.version;
  } catch {
    return "unknown";
  }
}

function main() {
  const version = getVersion();
  console.log(`\npi-interview v${version}`);
  console.log("Installing to:", TARGET_DIR);
  console.log("");

  ensureDir(TARGET_DIR);

  // Copy individual files
  for (const file of FILES_TO_COPY) {
    const src = path.join(SOURCE_DIR, file);
    const dest = path.join(TARGET_DIR, file);
    if (fs.existsSync(src)) {
      copyFile(src, dest);
      console.log("  Copied:", file);
    }
  }

  // Copy directories
  for (const dir of DIRS_TO_COPY) {
    const src = path.join(SOURCE_DIR, dir);
    const dest = path.join(TARGET_DIR, dir);
    if (fs.existsSync(src)) {
      copyDir(src, dest);
      console.log("  Copied:", dir + "/");
    }
  }

  console.log("");
  console.log("Installation complete!");
  console.log("Restart pi to load the extension.");
  console.log("");
}

main();
