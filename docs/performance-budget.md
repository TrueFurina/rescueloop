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
- Native artifact callbacks use a 1,024-path bounded queue. Overflow triggers a recursive
  reconciliation whose scanner streams through a 256-path bounded channel, so recovery does not
  materialize the watched directory tree in memory.
- Watcher tasks share a cancellation token. Ctrl-C and Unix SIGTERM stop collectors and heartbeat,
  drain the bounded persistence queue for up to 30 seconds, and join every task before exit.
- Windows also listens for Ctrl-Break, console close, user logoff and system shutdown events through
  native Tokio console-control streams before entering the same bounded drain path.
- AI analysis, hashing, repair planning and verification run only after an incident or explicit user
  action; none run during idle.
- Duplicate artifacts use deterministic IDs and atomic creation.
- Docker waits for native socket-creation events when the engine is offline; it does not spawn the
  CLI on a timer. A connected engine uses one blocking event stream.
- Container event records are capped at 64 KiB and oversized lines are drained through the next
  newline. `inspect` and diagnostic-log output are drained but retain at most 256 KiB each, with a
  five-second deadline and process-group termination on Unix. Restart history retains at most 4,096
  active container IDs and expires inactive windows after 60 seconds.
- macOS Unified Log is activated only in an authorized root daemon context. A normal user agent
  does not retry a source it cannot access.
- macOS Unified Log and Windows Event Log streams share the same 64 KiB bounded line reader as
  container events. Oversized records are discarded and the stream resynchronizes at the newline.
- Supervised-process stdout and stderr are fully drained to prevent child deadlock while retaining
  no more than 16 KiB per stream for bounded diagnostics.

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

On 2026-08-28, the production release build after bounded queues and graceful lifecycle changes
measured `0.007%` average CPU, `0.100%` maximum sampled CPU, 10.33 MiB average RSS and 10.34 MiB
peak RSS over a 30-sample macOS arm64 smoke benchmark. This proves comfortable short-run margin;
the required 1,800-second release gate and scheduled 24-hour soak remain authoritative.
