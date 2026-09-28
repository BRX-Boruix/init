# init

BORUIX's **PID 1** — the first user-space process in the system, taking it from "the kernel is ready" to "the system is usable".

[简体中文](README.md)

## What it does

| Phase | Work |
| --- | --- |
| Startup | Start the daemons and hardware drivers in a fixed order |
| Resident | Supervise those processes and restart them when needed |
| Terminals | Start a terminal daemon and a login program for each terminal instance |

## The startup order is deliberate

The order is not arbitrary, and there is one hard constraint: **the audio output device is exclusive**, and [`audiod`](https://github.com/BRX-Boruix/audiod) never releases it once claimed. So it must start **after `intel-hda` and before any other program wanting that device**.

A daemon that fails to start **does not block boot** — each is independent, and one failing does not affect the rest. Only terminal-related failures are serious (no terminal means no usable system).

## Terminal instances

The system can have several terminals, each independent. The instance count is injected at **build time** and is **the same value from the same source** as the number the kernel pre-creates:

```
the kernel pre-creates N terminal instances → init serves N terminal daemons and N switch positions
```

The two numbers must agree, or you get "the kernel created it but nobody serves it" or "somebody serves something whose device does not exist". So it has **exactly one source of truth** (injected at build time), with defensive clamping.

## Two session modes

| Mode | Behaviour |
| --- | --- |
| **Rotating** (default) | One terminal is served at a time; when a session ends it moves to the next |
| **Parallel** | Every terminal is served independently and simultaneously |

Rotating is the default because it demands the least memory and the fewest processes. Parallel suits scenarios needing several terminals at once.

## Creating terminals at runtime

A terminal instance can be created on demand at runtime through a **file protocol** — the requester creates a file in an agreed directory and init watches that directory and responds:

```
requester creates /system/console-requests/<n>   (n = the desired terminal number)
init scans the directory → validates the number → starts that terminal's daemon and login → records it → removes the request file
```

**No new system call was added for this.** Creating a request file is itself atomic, and duplicate requests are deduplicated for free: failing to create a file of the same name means "this terminal has already been requested".

An invalid request file is **honestly removed and recorded**, not silently ignored — discarding is a deliberate decision and must be auditable.

## The patrol does not spin

In parallel mode init must wake periodically to handle requests. It does so with a **timed wait**: if no child process changes state within the window, the timeout triggers one patrol. **The timeout is itself the maximum response delay for a terminal request** (200 milliseconds, acceptable for interaction).

## Building

```bash
cargo build --release
```

Build-time options available:

| Option | Meaning | Default |
| --- | --- | --- |
| Terminal instance count | How many terminals to pre-create at boot, clamped to 1..=256 | 4 |
| Session mode | Rotating or parallel | Rotating |

## Layout

```
init/
├── Cargo.toml    # package definition
├── build.rs      # injects the linker script and build-time options
├── linker.ld     # user-space section layout
└── src/
    └── main.rs   # startup sequence, supervision loop, terminal instance management
```

## Related projects

- [`shell`](https://github.com/BRX-Boruix/shell) — the user shell running after the system initialises
- [`login`](https://github.com/BRX-Boruix/login) — the login authentication program
- [`consoled`](https://github.com/BRX-Boruix/consoled) — the terminal byte producer daemon
- [`libsys`](https://github.com/BRX-Boruix/libsys) — the user-space syscall wrapper

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
