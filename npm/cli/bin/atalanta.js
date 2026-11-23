#!/usr/bin/env node
// Launcher for the prebuilt `atalanta` binary. npm installs only the
// platform package that matches this machine (each one's package.json has
// `os` and `cpu`), listed in this package's optionalDependencies; this
// script finds its binary and runs it with the same arguments and stdio.
"use strict";

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const SCOPE = "@atalantahq";
const PLATFORMS = {
  "darwin-arm64": `${SCOPE}/cli-darwin-arm64`,
  "darwin-x64": `${SCOPE}/cli-darwin-x64`,
  "linux-arm64": `${SCOPE}/cli-linux-arm64`,
  "linux-x64": `${SCOPE}/cli-linux-x64`,
};

function fail(message) {
  process.stderr.write(`atalanta: ${message}\n`);
  process.exit(1);
}

function binaryPath() {
  // An explicit binary wins (a local build, or a platform npm can't install).
  const override = process.env.ATALANTA_BINARY_PATH;
  if (override) {
    if (!fs.existsSync(override)) fail(`ATALANTA_BINARY_PATH=${override} does not exist`);
    return override;
  }
  const key = `${process.platform}-${process.arch}`;
  const pkg = PLATFORMS[key];
  if (!pkg) {
    fail(
      `no prebuilt binary for ${key}. Atalanta ships for macOS (arm64, x64) and Linux (x64, arm64); ` +
        "Windows is not supported. Other platforms: cargo install atalanta-cli",
    );
  }
  try {
    return require.resolve(`${pkg}/bin/atalanta`);
  } catch {
    fail(
      `the platform package ${pkg} is not installed. It is an optional dependency: ` +
        "reinstall without --omit=optional (a lockfile written on another platform can also " +
        "leave it out), or set ATALANTA_BINARY_PATH to a atalanta binary.",
    );
  }
}

const result = spawnSync(binaryPath(), process.argv.slice(2), { stdio: "inherit" });
if (result.error) fail(result.error.message);
if (result.signal) {
  // Die of the same signal, so a shell sees what the binary saw.
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
