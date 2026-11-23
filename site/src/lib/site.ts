import fs from 'node:fs';
import path from 'node:path';

/** Repo root: the parent of site/ (override with REPO_ROOT). */
export const REPO_ROOT = path.resolve(process.env.REPO_ROOT || path.join(process.cwd(), '..'));

function readVersion(): string {
  const toml = fs.readFileSync(path.join(REPO_ROOT, 'Cargo.toml'), 'utf8');
  const section = toml.split(/^\[workspace\.package\]\s*$/m)[1];
  const m = section?.split(/^\[/m)[0].match(/^version\s*=\s*"([^"]+)"/m);
  if (!m) throw new Error('Could not read [workspace.package] version from Cargo.toml');
  return m[1];
}

export const REPO_URL = 'https://github.com/atalantahq/atalanta';
export const RAW_URL = 'https://raw.githubusercontent.com/atalantahq/atalanta/main';
export const VERSION = readVersion();
export const RELEASE_STATE = 'released';
export const CONTACT = 'atalanta@atomicmail.io';
export const LICENSE = 'AGPL-3.0';
export const SITE_NAME = 'Atalanta';
export const TAGLINE = 'Android, iOS, web and API tests in YAML, one static binary, written in Rust.';
