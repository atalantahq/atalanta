# Security Policy

## Supported versions

Velocity is pre-1.0. Security fixes go into the latest minor release line only.

| Version | Supported |
| ------- | --------- |
| 0.2.x   | Yes       |
| < 0.2   | No        |

Fixes ship as a new patch release (for example 0.2.3). Upgrade to the latest
0.2.x to get them. When 0.3.0 is released, 0.2.x keeps getting fixes for
critical issues for 30 days and then stops.

## Reporting a vulnerability

**Please do not open a public issue, pull request or discussion for a
security problem.**

Report it privately through GitHub's private vulnerability reporting:

1. Go to the repository's **Security** tab.
2. Click **Report a vulnerability**.
3. Fill in the advisory form.

Only the maintainers can see the report. Please include:

- the Velocity version (`velocity --version`) and how you installed it
  (release archive, Homebrew, npm, `cargo install`, source build);
- the platform: host OS, and the device, simulator, emulator or browser involved;
- the component, for example the `velocity inspect` console, the MCP server,
  the Android tree or video server, the iOS runner, the web driver, AI
  provider calls, or report and artifact output;
- steps to reproduce, or a proof of concept, and the impact you expect
  (for example "a web page I visit can tap on my emulator");
- whether you plan to disclose it publicly, and when.

If you can't use GitHub's reporting form, open an issue that asks for a
private contact channel. Don't include any details of the problem in it.

## What to expect

These are targets for a project maintained by volunteers, not guarantees:

| Step | Target |
| ---- | ------ |
| Acknowledge the report | within 3 business days |
| First assessment (accepted or declined, with a severity) | within 10 business days |
| Fix released for a critical or high-severity issue | within 30 days of the assessment |
| Fix released for a medium or low-severity issue | in the next scheduled release, normally within 90 days |

We will keep you informed while we work on it. We will agree a disclosure
date with you, publish a GitHub Security Advisory (with a CVE where
appropriate) when the fix ships, and credit you unless you'd rather not be
named.

## Scope

In scope: code in this repository and the artifacts built from it, which are
the `velocity` binary, the release archives and installers, the embedded
Android tree and video server (`velocity-tree.jar`), the iOS VelocityRunner,
the in-app sync probes and the Flutter bridge. Examples:

- a way for a web page, another user on the same machine, or a host on the
  network to drive a device, read the screen, run commands, or read or
  write files through any server Velocity starts. These are the inspector
  console, the tree, video and sync servers, the iOS runner, and their
  `adb`/usbmuxd port forwards;
- bypassing the inspector's token, `Host` or `Origin` checks;
- path traversal or overwriting files outside the project, flows or results
  directories;
- secrets (API keys, `${VAR}` values marked secret) showing up unredacted in
  reports, traces, logs, MCP replies or requests to AI providers;
- model output that turns into a selector, a typed value or a command
  without the checks described in `docs/SECURITY.md`;
- a vulnerable dependency that is reachable in practice.

Out of scope:

- what the system under test does. Velocity drives the app you point it at,
  with the access the device grants (`adb` shell, XCTest, Chrome DevTools).
  Someone who can already run `adb`, `xcrun` or Velocity commands as you
  already has that control;
- risks the documentation describes as accepted, such as binding the
  inspector to a non-loopback address with `--host`, or a cabled iPhone's
  runner listening on Wi-Fi without `ios.runnerToken`. A way around the
  protection those modes still have *is* in scope;
- what AI providers do with the data you choose to send them (see
  `docs/AI-PROVIDERS.md`);
- third-party tools Velocity calls but does not ship, such as `adb`,
  Xcode, WebDriverAgent, Chrome and the Android emulator. Please report
  those upstream;
- denial of service that needs local access or a test suite you wrote
  yourself.

## Verifying what you install

- Release archives are listed with their sha256 in `SHA256SUMS`, and carry
  GitHub build attestations. `install.sh` checks the archive against
  `SHA256SUMS` (and against `--sha256 HEX` when given) before installing;
  `--verify-attestation` also checks the provenance with `gh attestation verify`.
- `SHA256SUMS` itself is signed: the release workflow attests it through
  Sigstore (keyless, bound to `.github/workflows/release.yml` at the tag),
  and ships the signed bundle as `SHA256SUMS.sigstore.json`. Check it with
  `gh attestation verify SHA256SUMS --repo omkar-dev/velocity`, or offline
  with `--bundle SHA256SUMS.sigstore.json`. Since every archive and SBOM is
  listed in it, a matching checksum then vouches for the file.
- Each release carries CycloneDX SBOMs: `velocity-<version>-velocity-cli.cdx.json`
  (every crate compiled into the binary) and
  `velocity-<version>-inspector-frontend.cdx.json` (the embedded web UI's npm
  packages), attested like the archives. The binaries are built with
  `cargo auditable`, so the crate list is also embedded in the binary
  itself: `cargo audit bin "$(command -v velocity)"` (or syft / grype,
  which read the same section) checks an installed binary against
  advisories published after it was built.
- npm packages are published with npm provenance (`npm publish --provenance`);
  `npm audit signatures` checks it.
- **macOS:** the release binaries are ad-hoc signed, not Developer ID signed
  or notarised. `curl` does not quarantine what it downloads, so normally
  there is nothing to do. If the installed binary does carry the
  `com.apple.quarantine` flag, `install.sh` removes it, only after the
  checksum matched, and says so; Gatekeeper would otherwise refuse to run
  it. Pass `--keep-quarantine` (or `VELOCITY_KEEP_QUARANTINE=1`) to leave
  the flag in place and approve the binary yourself in System Settings >
  Privacy & Security. A report that a tampered binary passes this check is
  in scope.

## The Android tree server handshake

The tree server Velocity starts on an Android device (protocol 18) listens
on an abstract socket whose name any app can bind. Before anything is sent,
the host challenges it: `HELLO <nonce>`, answered with an HMAC-SHA256 of
the nonces under a per-start token only the shell user can read. Only then
does the host authenticate with an HMAC of its own (`AUTH`); the token never
crosses the socket. A server that can't prove it holds the token gets
nothing, and the host reports a squatting app. Third-party tools that talk to
`@velocity_tree` directly must do the same handshake. The details are in
`docs/SECURITY.md` section 3; a way to make the host send a command or a
typed value to a process that never proved the token is in scope.

`docs/SECURITY.md` describes the threat model for each network-facing part
of Velocity, the defences it has, and the risks that remain.

## Automated security checks

`.github/workflows/security.yml` runs these on pull requests that touch
code, dependencies, workflows or the check configuration, on every push to
`main`, weekly (Mondays), and from the Actions tab. Each job calls
`scripts/security-check.sh <check>`, so a local run does exactly what CI
does, with the same allowlists:

| Check | Tool (pinned in security.yml) | Fails on | Allowlist / config |
|---|---|---|---|
| `deny` | cargo-deny | a RustSec advisory (vulnerable, unmaintained, unsound, yanked) | `deny.toml` `[advisories] ignore` |
| `osv` | OSV-Scanner, every lockfile (`Cargo.lock`, `fuzz/Cargo.lock`, the frontend's `package-lock.json`) | any known vulnerability | `osv-scanner.toml` |
| `npm-audit` | `npm audit --audit-level=high`, production and dev dependencies reported separately | high or critical | none: fix it, or explain it in `osv-scanner.toml` and here |
| `gitleaks` | gitleaks over the whole git history (and, locally, uncommitted changes) | a possible secret | `.gitleaks.toml` |
| `zizmor` | zizmor over `.github/workflows`, `action.yml`, `dependabot.yml` | low, medium or high findings | inline `# zizmor: ignore[rule]` with a reason |
| `semgrep` | Semgrep OSS packs: Rust, JavaScript/TypeScript/React, Python, Java, Bash, GitHub Actions (digest-pinned container in CI) | ERROR or WARNING findings | `.semgrepignore`, inline `nosemgrep: <rule-id>` with a reason |
| `unsafe` | `scripts/unsafe-inventory.py` | a crate with more `unsafe` than `scripts/unsafe-baseline.txt` | the baseline file |

Also in security.yml: CycloneDX SBOMs (`scripts/sbom.sh`, an artifact here,
release assets in release.yml), and, weekly and on demand, one minute of
cargo-fuzz per target (`fuzz/`: the suite YAML parser, the Android tree
dump, iOS page source, tree-server reply lines, the `http` step's JSONPath
and matchers, the Maestro converter). `scorecard.yml` runs OpenSSF
Scorecard weekly. Licences, bans and sources (`cargo deny check licenses
bans sources`) stay in ci.yml, where they block `CI OK`.

The repository is private on GitHub Free, so nothing is uploaded to code
scanning: a finding fails its job and is listed in the log and the job
summary, and each job keeps its SARIF report as an artifact for 30 days.
security.yml is not part of `CI OK`: an advisory published overnight turns
it red without blocking unrelated pull requests.

### Running them locally

```sh
brew install cargo-deny osv-scanner gitleaks zizmor semgrep   # once
scripts/security-check.sh                 # every check
scripts/security-check.sh gitleaks zizmor # some of them
scripts/security-check.sh sbom fuzz       # opt-in: SBOMs; FUZZ_SECONDS of each fuzz target
```

A tool that is not installed is skipped, with the command that installs
it. The script exits 1 when any check that ran found something; reports go
to `target/security/`. Fuzzing needs `rustup toolchain install nightly
--profile minimal` and `cargo install --locked cargo-fuzz`; on stable,
`RUSTFLAGS="--cfg fuzzing" cargo check --manifest-path fuzz/Cargo.toml`
type-checks the targets.

`scripts/install-hooks.sh` opts this clone into git hooks
(`scripts/hooks/`): pre-commit runs gitleaks on the staged changes plus
`cargo fmt --check`, the unsafe inventory, shellcheck, actionlint and
zizmor on what changed; pre-push runs gitleaks on the commits being pushed
and, when a lockfile changed, `deny`, `osv` and `npm-audit`.
`scripts/install-hooks.sh --remove` turns them off; `--no-verify` skips
them once.

### Triage: fixing or accepting a finding

Fix it when there is a fix: `cargo update -p <crate>` or
`npm audit fix` (then `python3 scripts/third-party-rust.py --write` for a
Cargo.lock change), a code change, or a workflow change. Accept it only
with a written reason, in the narrowest place the tool offers:

- **Rust advisory with no fix**: an `ignore` entry in `deny.toml` with
  `reason`, and the same id in `osv-scanner.toml` with `ignoreUntil` (six
  months at most, so it comes back for review).
- **npm advisory with no fix**: `osv-scanner.toml` with `ignoreUntil`;
  only for dev-only packages that never reach `dist/`. A high one in a
  production dependency is fixed, not ignored.
- **gitleaks**: a fake value (test fixture, RFC test vector) goes in
  `.gitleaks.toml` as an exact-value regex or a single file path, with a
  comment saying where it is used. **A real secret is never allowlisted**:
  rotate it first; rewriting history does not un-leak it, as clones and
  caches keep it.
- **zizmor**: `# zizmor: ignore[<rule>]` on the flagged line, with why it
  is safe there.
- **Semgrep**: `nosemgrep: <full-rule-id>` on (or just above) the line,
  with a comment giving the reason; `.semgrepignore` only for generated or
  recorded files. Rules dropped for the whole repository are listed with
  their reasons in `scripts/security-check.sh`.
- **unsafe**: a new `unsafe` block needs a `// SAFETY:` comment (clippy's
  `undocumented_unsafe_blocks`); after review, raise the crate's line with
  `python3 scripts/unsafe-inventory.py --write` in the same change.

Accepted findings as of 2026-11-11: RUSTSEC-2024-0449 (serde_yaml) and
RUSTSEC-2026-0192 (ttf-parser), both "unmaintained" with no successor yet;
GHSA-g3hv-75wg-m249 (minimatch, moderate, inside the frontend's eslint
tooling only); the fake tokens and test keys listed in `.gitleaks.toml`;
the zizmor and Semgrep annotations in the workflows and bench scripts.

### Dependabot

`.github/dependabot.yml` opens grouped weekly updates for Cargo, the
inspector frontend's npm packages and GitHub Actions (SHA pins with their
version comments), each held back until the release is 7 days old
(`cooldown`; security updates are not delayed). There is no Gradle or pub
entry: the sync probe compiles against a deliberate API floor (`compileOnly`
OkHttp 4.12, AGP and Kotlin from probes.yml), and the Flutter bridge has
only SDK dependencies.

A Cargo update changes `Cargo.lock`, so `docs/licenses/THIRD-PARTY-RUST.md`
must be regenerated in the same PR, and ci.yml's `Release files` job fails
until it is. Dependabot can't run the script itself; the failed job attaches
the regenerated file and prints the fix. Check out the PR branch and run:

```sh
python3 scripts/third-party-rust.py --write
git commit -am "Regenerate THIRD-PARTY-RUST.md" && git push
```

(A workflow could push that commit, but a push made with the default
`GITHUB_TOKEN` does not re-run CI, so the PR would be left without checks;
doing it automatically needs a GitHub App or PAT token.)

### If the repository goes public

These are free for public repositories and replace parts of the above:

1. **Code scanning with CodeQL** (Settings > Code security > Code scanning,
   default setup: Rust, JavaScript/TypeScript, Python, Java, Actions). Then
   upload the SARIF reports security.yml already writes
   (`github/codeql-action/upload-sarif`, `security-events: write`), so
   OSV-Scanner, zizmor, Semgrep and gitleaks findings appear in the
   Security tab.
2. **Dependency review** (`actions/dependency-review-action` on pull
   requests, `fail-on-severity: high`) and the dependency graph's
   **Dependabot alerts / security updates**.
3. **Secret scanning and push protection** (Settings > Code security): it
   blocks a push that contains a known token format, before it is in the
   history at all.
4. **Private vulnerability reporting** (Settings > Code security), which
   "Reporting a vulnerability" above relies on (public repositories only).
5. **OpenSSF Scorecard publishing**: `publish_results: true` and
   `id-token: write` in scorecard.yml, and the badge.
6. Consider **trusted publishing** (OIDC) for crates.io and npm instead of
   the long-lived `CARGO_REGISTRY_TOKEN` / `NPM_TOKEN` secrets.

### Hardened runners

Every job starts with `step-security/harden-runner` in `egress-policy:
audit` mode: it records each outbound connection (visible in the job's
StepSecurity insights link) without blocking anything; on macOS runners it
only logs that it is not supported. Once a few weeks of runs show the
endpoints a job needs, switch that job to blocking:

```yaml
      - uses: step-security/harden-runner@<sha> # vX.Y.Z
        with:
          egress-policy: block
          allowed-endpoints: >
            github.com:443
            api.github.com:443
            objects.githubusercontent.com:443
            static.rust-lang.org:443
            index.crates.io:443
            static.crates.io:443
```

Start with the jobs that hold secrets or write permissions (release.yml's
`release`, `npm`, `crates`, `homebrew`), where an exfiltration would hurt
most.

## GitHub settings checklist

For the repository owner to apply by hand (nothing in this repository
changes settings). Where a setting needs a paid plan for a private
repository, it says so.

- [ ] **Ruleset on `main`** (Settings > Rules > Rulesets; rulesets on
      private repositories need GitHub Pro or Team): require a pull request,
      require status check `CI OK`, require code-owner review
      (`.github/CODEOWNERS`), block force pushes and deletions, require
      linear history.
- [ ] Optionally also require the security.yml jobs that should block
      (for example `Secrets (gitleaks)` and `Workflow audit (zizmor)`;
      advisories are better left non-blocking).
- [ ] **Signed commits and tags**: require signed commits in the ruleset;
      add a tag ruleset for `v*` that restricts who can create tags and
      blocks their update and deletion (a release is whatever a `v*` tag
      points at).
- [ ] **Actions permissions** (Settings > Actions > General): allow only
      actions by GitHub, verified creators and a listed set
      (`dtolnay/rust-toolchain`, `Swatinem/rust-cache`,
      `taiki-e/install-action`, `EmbarkStudios/cargo-deny-action`,
      `dorny/paths-filter`, `step-security/harden-runner`,
      `ossf/scorecard-action`, `android-actions/setup-android`, `reactivecircus/android-emulator-runner`,
      `raven-actions/actionlint`, `subosito/flutter-action`,
      `gradle/actions`, `browser-actions/setup-chrome`), and tick "Require
      actions to be pinned to a full-length commit SHA".
- [ ] **Default `GITHUB_TOKEN` permissions: read** repository contents and
      packages only, and leave "Allow GitHub Actions to create and approve
      pull requests" off. Every workflow also declares its own
      `permissions:`.
- [ ] **Fork pull request workflows**: require approval for all outside
      collaborators; don't send secrets or write tokens to fork PR
      workflows.
- [ ] **Environments**: put the publish jobs' secrets (`CARGO_REGISTRY_TOKEN`,
      `NPM_TOKEN`, `HOMEBREW_TAP_TOKEN`) in an environment restricted to
      `v*` tags, with a required reviewer if the plan allows it.
- [ ] **Private vulnerability reporting** on (Settings > Code security).
      GitHub offers it for public repositories only: until the repository
      is public, reporters fall back to the "ask for a private contact
      channel" route above, so make sure issues are open and watched.
- [ ] **Dependabot**: alerts and security updates on (the dependency graph
      is available on private repositories).
- [ ] When public: **secret scanning with push protection**, CodeQL default
      setup and the rest of "If the repository goes public".
- [ ] Tokens: fine-grained, single-repository, shortest expiry that works
      (`HOMEBREW_TAP_TOKEN`: contents write on `omkar-dev/homebrew-velocity`
      only).

