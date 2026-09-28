# init

**简体中文** | [English](#english)

BORUIX 的 **PID 1 进程**——内核启动后拉起的第一个用户态进程，负责把整个系统从内核交接到可用的
运行状态。

---

## 它做什么

内核完成初始化后需要有人接手：挂载数据盘、把各种系统服务拉起来、管理登录会话，并在服务崩溃时
把它重新拉起。这就是 `init` 的职责。

### 启动服务

系统启动时，`init` 按顺序拉起这些常驻服务：

| 服务 | 职责 |
| --- | --- |
| `volumed` | 卷管理（发现并挂载存储） |
| `driverd` | 运行时驱动自动装载 |
| `userd` | 用户账户管理 |
| `intel-hda` | 声卡驱动 |
| `audiod` | 音频混音 |
| `consoled` | 把键盘事件转成终端字节 |
| `login` | 登录认证 |

其中只有终端相关的服务（`consoled`、`login`）属于关键路径——它们失败会直接影响能否登录；
其余服务失败只记录并继续启动，不会让系统卡死。

### 启动自检

启动过程中，`init` 会对系统各层做一遍检查——读写文件、查询内核信息、验证内存管理、确认数据盘
内容真的能读出来。这些检查的输出会打印出来，**失败只记录、不中断启动**。

这样设计的原因是：一个自检失败不应该让系统完全无法启动。先让系统起来、能登录、能排查，比卡在
启动阶段更有用。

### 监督与会话管理

系统起来之后，`init` 进入**监督循环**，持续处理三件事：

- **服务崩溃重拉**——某个守护进程死了就把它重新拉起
- **会话管理**——管理登录会话的生命周期
- **终端实例创建**——按需创建新的终端实例

## 登录会话

用户通过 `login` 认证后才进入交互 shell。`init` 负责把 `login` 拉起并在其退出后重新拉起，
因此**永远有登录提示可用**。

会话有两种模式，在构建时选择：

| 模式 | 行为 |
| --- | --- |
| `serial`（默认） | 一次一个会话，前一个退出后才拉下一个 |
| `parallel` | 每个终端实例各有一个会话同时在场 |

### 按需创建终端实例

除了开机预创建的终端实例，系统还支持在运行时创建新实例。请求方通过**创建文件**来表达请求，
`init` 在监督循环中扫描请求目录并响应：合法请求会拉起的终端守护与登录程序，非法请求会被丢弃
并留下记录。

选择文件协议而非新增系统调用的原因是：文件的原子创建特性天然提供了去重——同名文件已存在即表示
该实例已被请求，竞争条件自然收敛，不需要额外的同步机制。

## 启动开关

`init` 接受几个命令行开关，用于自动化验证场景：

| 开关 | 作用 |
| --- | --- |
| `--selftest` | 非交互地跑一遍线程自检 |
| `--selftest-quick` | 非交互地跑一遍快速自检 |
| `--run=<命令行>` | 以非交互 shell 执行任意一条命令行 |

这些开关的用途是让验收流程能在**没有人工键盘输入**的情况下跑起来——正常启动时 shell 的输入来自
物理键盘，无法从串口日志驱动，所以需要一个绕过交互的入口。

> **注意**：在标准启动路径下，内核**不向 `init` 传递任何参数**，因此这些开关只能通过**构建期
> 环境变量**预置。同一机制也用于设置终端实例数量与会话模式。

## 构建

```bash
cargo build --target x86_64-unknown-none --release
```

### 构建期配置

| 环境变量 | 作用 | 默认 |
| --- | --- | --- |
| `BORUIX_CONSOLES_N` | 开机预创建的终端实例数（钳制在 1..=256） | 4 |
| `BORUIX_SESSION_MODE` | 会话模式（`serial` / `parallel`） | `serial` |
| `BORUIX_INIT_ARGS` | 预置上面那些启动开关 | 无 |

终端实例数需要**与内核侧的数值保持一致**——设备侧预创建几个实例，`init` 就提供几个对应的守护
与轮转位。两侧从同一个环境变量读取，保证不会各说各话。

## 可替换性

`init` 是一个**可替换的组件**——它不是一个特权固化的特殊程序，而是一个约定：内核拉起它，它按
约定启动其余服务。换一个实现了同样职责的 `init` 应该同样可行。

## 文件结构

```
init/
├── Cargo.toml    # 包定义
├── build.rs      # 注入链接脚本 + 构建期配置
├── linker.ld     # 用户态段布局
└── src/
    └── main.rs   # 程序本体
```

## 相关项目

- [`libsys`](https://github.com/BRX-Boruix/libsys) —— 用户态系统调用封装
- [`consoled`](https://github.com/BRX-Boruix/consoled) —— 终端字节生产者
- [`driverd`](https://github.com/BRX-Boruix/driverd) —— 驱动自动装载
- [`audiod`](https://github.com/BRX-Boruix/audiod) —— 音频混音守护进程

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。

---

# English

[简体中文](#init) | **English**

BORUIX's **PID 1 process** — the first user-space process started after the kernel boots, responsible
for handing the system over from the kernel into a usable running state.

---

## What it does

After the kernel finishes initialising, something has to take over: mount the data disk, bring up the
various system services, manage login sessions, and restart a service when it crashes. That is
`init`'s job.

### Starting services

At boot, `init` starts these resident services in order:

| Service | Role |
| --- | --- |
| `volumed` | Volume management (discovering and mounting storage) |
| `driverd` | Runtime driver auto-loading |
| `userd` | User account management |
| `intel-hda` | Sound card driver |
| `audiod` | Audio mixing |
| `consoled` | Turns keyboard events into terminal bytes |
| `login` | Login authentication |

Only the terminal-related services (`consoled`, `login`) are on the critical path — their failure
directly affects whether anyone can log in. The others only log and let boot continue when they fail,
so the system does not wedge.

### Boot self-checks

During startup `init` runs a pass of checks across the system's layers — reading and writing files,
querying kernel information, verifying memory management, and confirming that the data disk's
contents can actually be read. The output is printed, and **failures are recorded without halting
boot**.

That is deliberate: one failing check should not leave the system completely unable to start. Getting
the system up so someone can log in and investigate beats wedging during startup.

### Supervision and session management

Once the system is up, `init` enters a **supervision loop** handling three things continuously:

- **Restarting crashed services** — if a daemon dies, bring it back
- **Session management** — managing the lifecycle of login sessions
- **Terminal instance creation** — creating new terminal instances on demand

## Login sessions

A user only reaches an interactive shell after authenticating through `login`. `init` starts
`login` and restarts it when it exits, so **a login prompt is always available**.

Sessions come in two modes, chosen at build time:

| Mode | Behaviour |
| --- | --- |
| `serial` (default) | One session at a time; the next starts when the previous exits |
| `parallel` | Each terminal instance has its own concurrent session |

### Creating terminal instances on demand

Beyond the instances pre-created at boot, the system supports creating new instances at run time. A
requester expresses the request by **creating a file**, and `init` scans the request directory in its
supervision loop and responds: valid requests get a terminal daemon and a login program, invalid ones
are discarded and recorded.

A file protocol was chosen over adding a system call because a file's atomic creation provides
deduplication for free — the same-named file already existing means that instance has already been
requested, so the race condition collapses without a separate synchronisation mechanism.

## Startup switches

`init` accepts a few command-line switches for automated verification scenarios:

| Switch | Effect |
| --- | --- |
| `--selftest` | Runs the thread self-test non-interactively |
| `--selftest-quick` | Runs the quick self-test non-interactively |
| `--run=<command line>` | Runs an arbitrary command line through a non-interactive shell |

These exist so verification flows can run **without a human at the keyboard** — on a normal boot the
shell's input comes from the physical keyboard and cannot be driven from a serial log, so a way to
bypass the interactive path is needed.

> **Note**: on the standard boot path the kernel passes `init` **no arguments at all**, so these
> switches can only be preset through a **build-time environment variable**. The same mechanism also
> sets the terminal instance count and the session mode.

## Building

```bash
cargo build --target x86_64-unknown-none --release
```

### Build-time configuration

| Environment variable | Effect | Default |
| --- | --- | --- |
| `BORUIX_CONSOLES_N` | Terminal instances pre-created at boot (clamped to 1..=256) | 4 |
| `BORUIX_SESSION_MODE` | Session mode (`serial` / `parallel`) | `serial` |
| `BORUIX_INIT_ARGS` | Presets the startup switches above | none |

The terminal instance count must **match the value on the kernel side** — as many instances as the
device side pre-creates, that many daemons and rotation slots `init` provides. Both sides read the
same environment variable, so they cannot disagree.

## Replaceability

`init` is a **replaceable component** — not a special program with hard-wired privilege, but an
agreement: the kernel starts it, and it starts the rest of the services per that agreement. A
different `init` implementing the same responsibilities should work equally well.

## Layout

```
init/
├── Cargo.toml    # package definition
├── build.rs      # injects the linker script + build-time configuration
├── linker.ld     # user-space section layout
└── src/
    └── main.rs   # the program itself
```

## Related projects

- [`libsys`](https://github.com/BRX-Boruix/libsys) — the user-space syscall wrapper
- [`consoled`](https://github.com/BRX-Boruix/consoled) — the terminal byte producer
- [`driverd`](https://github.com/BRX-Boruix/driverd) — driver auto-loading
- [`audiod`](https://github.com/BRX-Boruix/audiod) — the audio mixing daemon

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
