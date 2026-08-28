import { readFileSync, writeFileSync } from "node:fs";

const semver = /^\d+\.\d+\.\d+$/;
const args = process.argv.slice(2);
const checking = args[0] === "--check";
const requestedVersion = checking ? args[1] : args[0];

const files = {
  cargo: "Cargo.toml",
  lock: "Cargo.lock",
  homebrew: "packaging/homebrew/rescueloop.rb",
  releaseWorkflow: ".github/workflows/release.yml",
  wingetVersion: "packaging/winget/RescueLoop.version.yaml",
  wingetInstaller: "packaging/winget/RescueLoop.installer.yaml",
  wingetLocale: "packaging/winget/RescueLoop.yaml",
};

function workspaceVersion() {
  const cargo = readFileSync(files.cargo, "utf8");
  const workspace = cargo.match(/\[workspace\.package\][\s\S]*?\r?\nversion = "([^"]+)"/);
  if (!workspace) throw new Error("workspace package version is missing from Cargo.toml");
  return workspace[1];
}

function replaceRequired(path, pattern, replacement) {
  const before = readFileSync(path, "utf8");
  if (!pattern.test(before)) throw new Error(`version field was not found in ${path}`);
  const after = before.replace(pattern, replacement);
  if (before !== after) writeFileSync(path, after);
}

function readRequired(path, pattern) {
  const match = readFileSync(path, "utf8").match(pattern);
  if (!match) throw new Error(`version field was not found in ${path}`);
  return match[1];
}

function assertEqual(label, actual, expected) {
  if (actual !== expected) {
    throw new Error(`${label} uses ${actual}; expected ${expected}`);
  }
}

if (!checking) {
  if (!requestedVersion || !semver.test(requestedVersion)) {
    throw new Error("usage: node scripts/sync-version.mjs <major.minor.patch>");
  }

  replaceRequired(
    files.cargo,
    /(\[workspace\.package\][\s\S]*?\r?\nversion = ")[^"]+("\r?\n)/,
    `$1${requestedVersion}$2`,
  );

  const localPackages = [
    "rescueloop",
    "rescueloop-agent",
    "rescueloop-core",
    "rescueloop-index",
    "rescueloop-ledger",
    "rescueloop-platform",
    "rescueloop-repair",
  ];
  let lock = readFileSync(files.lock, "utf8");
  for (const name of localPackages) {
    const pattern = new RegExp(`(name = "${name}"\\r?\\nversion = ")[^"]+("\\r?\\n)`);
    if (!pattern.test(lock)) throw new Error(`${name} is missing from Cargo.lock`);
    lock = lock.replace(pattern, `$1${requestedVersion}$2`);
  }
  writeFileSync(files.lock, lock);

  replaceRequired(files.homebrew, /(^  version ")[^"]+("$)/m, `$1${requestedVersion}$2`);
  replaceRequired(
    files.releaseWorkflow,
    /(^        default: ")[^"]+("$)/m,
    `$1${requestedVersion}$2`,
  );
  replaceRequired(files.wingetVersion, /(^PackageVersion: ).+$/m, `$1${requestedVersion}`);
  replaceRequired(files.wingetInstaller, /(^PackageVersion: ).+$/m, `$1${requestedVersion}`);
  replaceRequired(files.wingetLocale, /(^PackageVersion: ).+$/m, `$1${requestedVersion}`);
  replaceRequired(
    files.wingetInstaller,
    /(releases\/download\/v)[^/]+(\/rescueloop-windows-x86_64\.zip)/,
    `$1${requestedVersion}$2`,
  );
}

const version = workspaceVersion();
if (!semver.test(version)) throw new Error(`invalid workspace version: ${version}`);
if (requestedVersion) assertEqual("requested release", version, requestedVersion);

assertEqual(
  "Homebrew formula",
  readRequired(files.homebrew, /^  version "([^"]+)"$/m),
  version,
);
assertEqual(
  "release workflow default",
  readRequired(files.releaseWorkflow, /^        default: "([^"]+)"$/m),
  version,
);
assertEqual(
  "Winget version manifest",
  readRequired(files.wingetVersion, /^PackageVersion: (.+)$/m),
  version,
);
assertEqual(
  "Winget installer manifest",
  readRequired(files.wingetInstaller, /^PackageVersion: (.+)$/m),
  version,
);
assertEqual(
  "Winget locale manifest",
  readRequired(files.wingetLocale, /^PackageVersion: (.+)$/m),
  version,
);
assertEqual(
  "Winget installer URL",
  readRequired(files.wingetInstaller, /releases\/download\/v([^/]+)\//),
  version,
);

const lock = readFileSync(files.lock, "utf8");
for (const name of [
  "rescueloop",
  "rescueloop-agent",
  "rescueloop-core",
  "rescueloop-index",
  "rescueloop-ledger",
  "rescueloop-platform",
  "rescueloop-repair",
]) {
  const match = lock.match(new RegExp(`name = "${name}"\\r?\\nversion = "([^"]+)"`));
  if (!match) throw new Error(`${name} is missing from Cargo.lock`);
  assertEqual(`${name} in Cargo.lock`, match[1], version);
}

console.log(`RescueLoop version ${version} is synchronized.`);
