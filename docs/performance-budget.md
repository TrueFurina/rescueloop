# Performance budget

RescueLoop's background detector has a hard idle target of less than 1% of one CPU core. The target
is a release gate, not an architectural assumption.

## Current controls

- Crash artifact collection is event-driven: FSEvents on macOS and ReadDirectoryChangesW on Windows
  through the native `notify` backend.
- The detector performs no periodic recursive directory scan.
- Artifact parsing is bounded to 40 allowlisted diagnostic lines of at most 500 characters each.
- Crash artifacts are streamed on the blocking pool: at most 1 MiB is retained for parsing while
  SHA-256 still covers the complete file. The in-memory artifact dedupe cache is capped at 4,096 paths.
- The collector-to-persistence queue is capped at 256 incidents and applies backpressure during bursts.
- AI analysis, hashing, repair planning and verification run only after an incident or explicit user
  action; none run during idle.
- Duplicate artifacts use deterministic IDs and atomic creation.
- Docker waits for native socket-creation events when the engine is offline; it does not spawn the
  CLI on a timer. A connected engine uses one blocking event stream.
- macOS Unified Log is activated only in an authorized root daemon context. A normal user agent
  does not retry a source it cannot access.

## Budgets

| Mode | CPU target | Memory target |
|---|---:|---:|
| Background idle | <1% of one core | <30 MiB RSS |
| Console idle | <1% of one core | <40 MiB RSS |
| Incident burst | bounded and returns to idle | <100 MiB RSS excluding AI CLI |

Every future flight-recorder source must document its sampling/event strategy and pass a 30-minute
idle benchmark on supported Windows and macOS versions. The AI CLI runs as a separate on-demand
process and is measured separately from the background detector.

Run the repeatable release gate with:

```sh
cargo build --release -p rescueloop
scripts/benchmark-idle.sh 1800
```

## Measured results

On macOS arm64, an unoptimized development build using the event-driven DiagnosticReports collector
showed `0.0%` idle CPU and approximately `3 MiB` RSS in a short three-sample `top` check. This is not
a substitute for the release benchmark.

On macOS arm64, the optimized multi-source watcher with DiagnosticReports enabled and Docker
installed but offline measured `0.0%` CPU in 10/10 one-second samples, approximately 9.3 MiB RSS,
and zero child processes. Docker socket discovery was event-driven during this measurement.
