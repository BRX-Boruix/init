# init

BORUIX's init process: the first user-space program, taking the system from "kernel ready" to "usable".

[简体中文](README.md)

## What it does

- Starts the daemons and hardware drivers in a fixed order
- Supervises them for the lifetime of the system: a daemon that exits is restarted under its original instance number
- Starts the console daemon and login program for each terminal instance
- Handles run-time requests for new terminals

## Terminal instances

The instance count is injected at build time (environment variable `BORUIX_CONSOLES_N`, default 4,
clamped to 1..=256) and shares one source of truth with the kernel: however many instances the kernel
pre-creates, init serves that many.

Two session modes:

- **Rotating** (default) — one terminal is served at a time; when a session ends the next one takes over
- **Parallel** — every terminal is served independently and simultaneously

## Run-time new terminals

A requester creates a file named after the desired number under `/system/console-requests/`; init
patrols that directory: validates the number, starts that terminal's daemon and login, registers it,
and deletes the request file. No new system call is involved — the atomicity of file creation
provides request deduplication for free.

Invalid request files are deleted and recorded, never silently ignored.

## Known limitations

- Request file names must be plain numbers from 1 to 63; 0 is the system console alias and cannot be requested
- The patrol period is 200 ms, which is also the maximum response latency for a new-terminal request
- When no audio consumer exists the mixing daemon is skipped with an honest note

## Building

```bash
cargo build --release
```

## Repository layout

```
init/
├── Cargo.toml    # package manifest
├── build.rs      # injects the linker script and build-time options
├── linker.ld     # user-space segment layout
└── src/
    └── main.rs   # boot sequence, supervision loop, terminal management
```

## Related projects

- [`shell`](https://github.com/BRX-Boruix/shell) — the user shell run after init completes
- [`login`](https://github.com/BRX-Boruix/login) — login authentication
- [`consoled`](https://github.com/BRX-Boruix/consoled) — the console byte producer
- [`openvt`](https://github.com/BRX-Boruix/openvt) — the user entry for requesting a new terminal
- [`libsys`](https://github.com/BRX-Boruix/libsys) — user-space syscall wrappers

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
