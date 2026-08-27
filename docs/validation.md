# Validation matrix

## macOS arm64

- Workspace unit tests and Clippy with warnings denied: passed.
- Native DiagnosticReports source: real SIGABRT report detected.
- Docker event stream through Colima: real `die`, `health_status: unhealthy`, and `oom` events detected.
- Restart loop: three exits grouped into one incident with `occurrence_count = 3` and upgraded classification.
- OOM race: Docker `oom` and `die` grouped into one `OutOfMemory` incident.
- Container repair: exact evidenced container restarted, verified running, receipt persisted, lifecycle marked fixed.
- File quarantine, cache regeneration, JSON patch, and POSIX permission rollback: tested.
- User LaunchAgent: installed, running with Homebrew PATH, then uninstalled cleanly.
- System LaunchDaemon: non-root refusal tested; privileged installation requires explicit `sudo`.
- Unsigned local `.pkg`: built successfully with `pkgbuild`; signing/notarization awaits publisher credentials.
- Short soak harness: watcher remained alive for 8 seconds at 0.2125% average CPU.
- Release idle benchmark, Docker installed/offline: latest local rerun averaged 0.013% CPU,
  peaked at 0.400% CPU, and used about 9.32 MiB RSS.

## Windows

- Core, platform, and repair crates cross-check for `aarch64-pc-windows-msvc`: passed.
- Windows Event Log subscription, Task Scheduler integration, service operations and supervised
  failure persistence are exercised by `scripts/e2e-windows.ps1` on native `windows-latest` CI.
- macOS cannot execute the Windows binary locally; the native runner remains authoritative.
