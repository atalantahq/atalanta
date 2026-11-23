#!/usr/bin/env node
// Assemble the npm packages for one release from its tarballs:
//
//   node npm/assemble.mjs --version 0.2.0 --artifacts <dir> [--out npm/dist]
//   node npm/assemble.mjs --version 0.2.0 --binary darwin-arm64=target/release/atalanta
//
// <dir> holds atalanta-<version>-<target>.tar.gz (the release assets).
// --binary takes a binary directly (a local build; repeatable), for testing
// with `npm pack`. Writes <out>/cli (the launcher package, its version and
// optionalDependencies stamped) and <out>/cli-<os>-<cpu> for every platform
// found. Every platform is required unless --allow-missing is given.
// Then: for d in <out>/*; do npm publish "$d" --access public; done
// (platform packages first; the release workflow does this).
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");

// npm platform key -> Rust target of the release tarball.
const TARGETS = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "linux-x64": "x86_64-unknown-linux-musl",
};

function die(msg) {
  console.error(`assemble: ${msg}`);
  process.exit(1);
}

const args = { binaries: {}, out: path.join(here, "dist"), allowMissing: false };
const argv = process.argv.slice(2);
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  const val = () => argv[++i] ?? die(`${a} needs a value`);
  if (a === "--version") args.version = val().replace(/^v/, "");
  else if (a === "--artifacts") args.artifacts = path.resolve(val());
  else if (a === "--out") args.out = path.resolve(val());
  else if (a === "--allow-missing") args.allowMissing = true;
  else if (a === "--binary") {
    const [key, file] = val().split("=");
    if (!TARGETS[key] || !file) die(`--binary wants <${Object.keys(TARGETS).join("|")}>=<path>`);
    args.binaries[key] = path.resolve(file);
  } else die(`unknown argument ${a}`);
}
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(args.version ?? "")) die("--version X.Y.Z is required");
if (!args.artifacts && Object.keys(args.binaries).length === 0) die("give --artifacts <dir> or --binary");

const main = JSON.parse(fs.readFileSync(path.join(here, "cli", "package.json"), "utf8"));
const scope = main.name.split("/")[0];

fs.rmSync(args.out, { recursive: true, force: true });
fs.mkdirSync(args.out, { recursive: true });

function binaryFor(key) {
  if (args.binaries[key]) return args.binaries[key];
  if (!args.artifacts) return null;
  const base = `atalanta-${args.version}-${TARGETS[key]}`;
  const tarball = path.join(args.artifacts, `${base}.tar.gz`);
  if (!fs.existsSync(tarball)) return null;
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "atalanta-npm-"));
  execFileSync("tar", ["-xzf", tarball, "-C", tmp]);
  const bin = path.join(tmp, base, "atalanta");
  if (!fs.existsSync(bin)) die(`${tarball} has no ${base}/atalanta`);
  return bin;
}

const built = [];
for (const [key, target] of Object.entries(TARGETS)) {
  const bin = binaryFor(key);
  if (!bin) {
    if (args.allowMissing) continue;
    die(`no binary for ${key} (${target})`);
  }
  const [platform, cpu] = key.split("-");
  const name = `${scope}/cli-${key}`;
  const dir = path.join(args.out, `cli-${key}`);
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  fs.copyFileSync(bin, path.join(dir, "bin", "atalanta"));
  fs.chmodSync(path.join(dir, "bin", "atalanta"), 0o755);
  fs.copyFileSync(path.join(root, "LICENSE"), path.join(dir, "LICENSE"));
  fs.copyFileSync(path.join(root, "NOTICE"), path.join(dir, "NOTICE"));
  // NOTICE points at the third-party inventories by these paths.
  fs.mkdirSync(path.join(dir, "docs", "licenses"), { recursive: true });
  for (const f of ["THIRD-PARTY-RUST.md", "THIRD-PARTY-NPM.md"]) {
    fs.copyFileSync(path.join(root, "docs", "licenses", f), path.join(dir, "docs", "licenses", f));
  }
  const pkg = {
    name,
    version: args.version,
    description: `The atalanta binary for ${key} (${target}). Install ${main.name} instead.`,
    homepage: main.homepage,
    repository: main.repository,
    license: main.license,
    os: [platform],
    cpu: [cpu],
    files: ["bin/atalanta", "LICENSE", "NOTICE", "docs/licenses"],
    preferUnplugged: true,
  };
  fs.writeFileSync(path.join(dir, "package.json"), `${JSON.stringify(pkg, null, 2)}\n`);
  fs.writeFileSync(path.join(dir, "README.md"), `# ${name}\n\nThe \`atalanta\` binary for ${key}. Install [\`${main.name}\`](https://www.npmjs.com/package/${main.name}); it picks this package on ${key}.\n`);
  built.push(name);
}

const cliDir = path.join(args.out, "cli");
fs.mkdirSync(path.join(cliDir, "bin"), { recursive: true });
fs.copyFileSync(path.join(here, "cli", "bin", "atalanta.js"), path.join(cliDir, "bin", "atalanta.js"));
fs.chmodSync(path.join(cliDir, "bin", "atalanta.js"), 0o755);
fs.copyFileSync(path.join(root, "LICENSE"), path.join(cliDir, "LICENSE"));
fs.copyFileSync(path.join(here, "README.md"), path.join(cliDir, "README.md"));
main.version = args.version;
main.files = ["bin/atalanta.js", "LICENSE", "README.md"];
for (const dep of Object.keys(main.optionalDependencies)) main.optionalDependencies[dep] = args.version;
fs.writeFileSync(path.join(cliDir, "package.json"), `${JSON.stringify(main, null, 2)}\n`);

console.log(`assembled ${main.name}@${args.version} with ${built.length} platform package(s) in ${args.out}`);
for (const b of built) console.log(`  ${b}`);
