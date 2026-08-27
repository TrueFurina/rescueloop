# Releasing RescueLoop

## Automated artifacts

Push a version tag such as `v0.1.0`. GitHub Actions builds macOS arm64/x86_64 tarballs and `.pkg`
installers, Windows x86_64 zip and `.msi` installers, and a `SHA256SUMS` manifest.

Homebrew and WinGet templates live under `packaging/`. Replace version/checksum placeholders from
`SHA256SUMS`, then submit them to the appropriate package repository.

## Signing credentials

Configure these GitHub Actions secrets to enable platform signing:

- `APPLE_CERTIFICATE_P12`: base64 Developer ID Application certificate;
- `APPLE_CERTIFICATE_PASSWORD`: certificate password;
- `APPLE_SIGNING_IDENTITY`: exact Developer ID Application identity;
- `APPLE_INSTALLER_IDENTITY`: exact Developer ID Installer identity;
- `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD`: notarization credentials;
- `WINDOWS_CERTIFICATE_PFX`: base64 Authenticode certificate;
- `WINDOWS_CERTIFICATE_PASSWORD`: certificate password.

Apple notarization additionally requires an Apple developer team and notary credentials. Secrets
must never be committed. Without them the workflow produces checksummed but unsigned artifacts.

## Update channel

Bootstrap installers default to GitHub's `latest` release and verify SHA-256. Pin a version with
`RESCUELOOP_VERSION=v0.1.0`. The detector itself does not poll the network for updates, preserving
the privacy and idle CPU budget. Automatic rollout belongs to Homebrew, WinGet, or enterprise policy.

## Validation

Regular CI runs all tests and a native Windows E2E. Weekly soak CI runs for 24 hours and fails if the
watcher exits or exceeds 1% average CPU. A manual 72-hour run is:

```sh
cargo build --release -p rescueloop
./scripts/soak.sh 259200
```
