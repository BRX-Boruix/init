//! BORUIX PID 1：首个用户态进程（init）。
//!
//! 依赖 libsys 的薄封装调用 syscall。作为 M4.4 的真实用户程序，验证
//! 「Rust no_std 用户程序 → 编译成 ELF → 内核加载运行」完整链路。
//!
//! 入口约定：libsys 提供 `_start`，调用本文件导出的 `user_main`，
//! 其返回值作为进程退出码。

#![no_std]
#![no_main]
// 临时允许：启动序列精简后，下列自检函数暂无调用点（即将迁往 `selftest` 命令的
// 宿主程序）。迁移完成后删除本允许，恢复“死代码即错误”的纪律。
#![allow(dead_code)]

use libsys::{brk, info, waitpid_any, write, yield_now, STDOUT};

/// 把无符号整数格式化为十六进制字符串（写入固定缓冲），返回有效切片。
///
/// 缓冲布局：`buf[0..2] = "0x"`，`buf[2..18]` 为 16 个 hex 位（高位在前）。
/// 去掉前导零，至少保留一位。
fn hex_u64(v: u64, buf: &mut [u8; 18]) -> &[u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nibble = (v >> ((15 - i) * 4)) & 0xF;
        buf[2 + i] = HEX[nibble as usize];
    }
    // 去掉前导零（buf[2..17] 可跳过），至少保留 buf[17] 最低位。
    let mut start = 2usize;
    while start < 17 && buf[start] == b'0' {
        start += 1;
    }
    &buf[..(20 - start)]
}

/// 输出一行 `label = value`（十六进制）。
fn print_hex(label: &[u8], v: u64) {
    let mut buf = [0u8; 18];
    let s = hex_u64(v, &mut buf);
    let _ = write(STDOUT, label);
    let _ = write(STDOUT, b" = ");
    let _ = write(STDOUT, s);
    let _ = write(STDOUT, b"\n");
}

/// A2：音频管道端到端**阻塞往返**测试（plan_audio_vfs.md 批次二）。
///
/// **为何放在 init 而非 shell 内建**：内核启动期测试（`test_audio_pipe_a2`）
/// 直接调节点与 ring，触达不到两条关键路径——AUDIO 域 syscall 包装本身、
/// 以及真正的阻塞-唤醒往返（`block_for_audio`/`wake_audio` 需要真实进程切换）。
/// 而 shell 内建依赖 stdin，本环境的 stdin 是 PS/2 键盘（非串口），无法自动驱动。
/// init 在启动时自动跑，使该验证成为**每次启动的常规回归**而非人工步骤。
///
/// **顺序是本测试的核心**（写反则等于什么都没验证）：
///   1. 先派生 consumer —— 它 attach 后在**空** ring 上调 fetch，真正入睡；
///   2. init 再经 VFS syscall 写入一帧 PCM —— 写路径的 notify 唤醒 consumer；
///   3. consumer 醒来逐字节校验、commit、detach，退出 0。
///
/// 失败不致命（非 fatal）：测试失败要**可见**，但不能让系统起不来。
/// 等待 intel-hda 完成 attach 的**时间上界**（毫秒）。
///
/// 【缺陷修正】原实现是 `AUDIO_ATTACH_WAIT_ROUNDS = 600` 次 `yield_now()`。
/// 那是**用调度轮次猜时间**，而轮次与真实时间没有固定换算：`yield_now()` 的
/// 代价取决于当时有多少就绪任务，4 核 SMP 下还受其它守护进程影响。
///
/// 实测后果（`--release`，`--systemdisk --redisk`）：600 轮在 intel-hda 完成
/// "认领控制器 -> 复位 -> 枚举 codec -> 能力查询 -> attach"之前就耗尽，于是
///
///     [init] audio ring consumer attached = false
///     [init] no audio consumer (no HDA device?); skipping audiod (honest skip)
///     [audio] pid=5 attached as PCM consumer        <- 驱动此时才 attach
///
/// init 判定"无消费者"并**永久跳过 audiod**——而 audiod 是唯一往 PCM ring 写
/// 数据的生产者。后果是 ring 永远空：驱动侧 `prefilled 0 bytes`、DMA 只播静音、
/// `stream_loop` 每 100ms 超时空转并刷屏。
///
/// 注意这不是"等得不够久"，而是**等待与真实时间脱钩**：无论把 600 改成多少，
/// 都无法保证覆盖——那只是换一个猜的数字（S13）。故改为等**真实时间**。
///
/// 取值依据：HDA 初始化要复位控制器、等 codec 枚举与 R1 能力查询，实测在
/// QEMU/TCG 下约 1-3 秒（`build` 出的 release 内核日志里，从 `[init] intel-hda
/// started` 到 `[audio] pid=5 attached` 之间还隔着 login 的启动）。取 15 秒：
/// 足够覆盖 TCG 慢速路径，又不至于在无 HDA 设备时让用户等太久。
/// 有界是刻意的：无 HDA 设备时**必须**能退出并如实跳过（S20 失败模式优先）。
const AUDIO_ATTACH_WAIT_MS: u64 = 15_000;
/// 轮询 `/devices/audio/dsp/status` 的间隔（毫秒）。
///
/// 它只影响**发现延迟**（attach 后多久开始派 audiod），不影响上界。
/// 取 20ms：相较 HDA 初始化的秒级耗时可忽略，又不至于密集占满调度。
const AUDIO_ATTACH_POLL_MS: u64 = 20;

/// 朴素子串查找（在 `hay` 中找 `needle`）。
///
/// 不引 JSON 解析：这里只需判定 `/devices/audio/dsp/status` 里是否出现
/// `"attached":true`。刻意保持最简——引入解析器会为一行判定增加大量代码与
/// 失败面。若将来需要读更多字段，再引入真正的解析。
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|i| &hay[i..i + needle.len()] == needle)
}

/// 判断 C 串是否以 `prefix` 开头（用于 `--run=<命令行>` 这类带值开关）。
///
/// 边界：只比较到 prefix 结束，不读串尾更远处——即便串比 prefix 短也安全返回 false。
fn arg_starts_with(p: *const u8, prefix: &[u8]) -> bool {
    let mut k = 0usize;
    while k < prefix.len() {
        // SAFETY: k < prefix.len()，逐字节比较；遇 NUL 立即返回 false。
        if unsafe { *p.add(k) } != prefix[k] {
            return false;
        }
        k += 1;
    }
    true
}

/// 取 C 串在 `prefix` 之后的剩余部分（调用方须先确认 `arg_starts_with`）。
/// 返回的切片借用 argv 原内存，生命周期与 argv 相同（无需复制）。
fn arg_rest(p: *const u8, prefix: &[u8]) -> &[u8] {
    let mut n = prefix.len();
    // SAFETY: p 为 NUL 结尾 C 串；n 停在 NUL 上。
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: [prefix.len(), n) 落在已确认的串范围内。
    unsafe { core::slice::from_raw_parts(p.add(prefix.len()), n - prefix.len()) }
}
/// 判断一个 NUL 结尾的 C 串是否**整个等于** `want`（逐字节比较，非子串匹配）。
///
/// 为何要抽成函数：argv 扫描此前把「求长度 + 比较」内联在循环里，加第二个开关就要
/// 复制一份——而这段代码全是裸指针算术，"复制一份"正是最容易引入越界的地方。
/// 抽出来后长度计算与比较各自只有一处，且 `n == want.len()` 同时充当越界守卫。
fn arg_eq(p: *const u8, want: &[u8]) -> bool {
    let mut n = 0usize;
    // SAFETY: p 指向 NUL 结尾 C 串；n <= want.len() 使读取不越过已知长度。
    while unsafe { *p.add(n) } != 0 && n <= want.len() {
        n += 1;
    }
    if n != want.len() {
        return false;
    }
    let mut k = 0usize;
    while k < n {
        // SAFETY: k < n <= want.len()，且已确认串长恰为 n，均在界内。
        if unsafe { *p.add(k) } != want[k] {
            return false;
        }
        k += 1;
    }
    true
}

/// init 主流程：打印信息、查询内核版本与堆断点、退出。
#[unsafe(no_mangle)]
pub extern "C" fn user_main(argc: isize, argv: *const *const u8) -> i32 {
    // argv 诊断开关（见 4.3.2 的说明）。
    // 为何用 argv 而非环境变量：libsys 无 environ 支持，而 argv 是既有机制
    // （shell 的非交互模式同款），无需新增子系统。
    //
    // 运行期开关（`--skip-hda` 已随 intel-hda 启动步一并取消，见 4.3.2 说明）：
    //   --selftest      ：以非交互 shell 跑一遍 `selftest thread`（ADR-038 U1 入口）
    //   --selftest-quick：以非交互 shell 跑一遍 `selftest quick`（A2-5 账户查询 E2E 入口）
    let mut run_selftest = false;
    let mut run_selftest_quick = false;
    // --run=<命令行>：以非交互 shell 执行**任意**一条命令行（A2-4/A2-5 通用验收入口）。
    let mut run_cmd: Option<&[u8]> = None;
    if argc > 0 && !argv.is_null() {
        let mut i = 0isize;
        while i < argc {
            // SAFETY: i < argc，argv 由内核 exec 路径按 C 数组构造，NUL 结尾。
            let p = unsafe { *argv.offset(i) };
            if !p.is_null() {
                if arg_eq(p, b"--selftest") { run_selftest = true; }
                else if arg_eq(p, b"--selftest-quick") { run_selftest_quick = true; }
                else if arg_starts_with(p, b"--run=") { run_cmd = Some(arg_rest(p, b"--run=")); }
            }
            i += 1;
        }
    }

    // 构建期预置开关（见 build.rs）：本环境 init 恒以**空 argv** 启动（内核生产
    // 路径不带 argv，且无 kernel cmdline），故运行期无法从外部传开关。
    // `BORUIX_INIT_ARGS` 由构建脚本作为编译期常量注入，使自动化构建能跑到
    // 需要非交互驱动的用户态验收（否则 shell 的 stdin 是 PS/2 键盘，串口日志
    // 驱动不了输入行）。运行期 argv 优先于构建期预置——两者可叠加。
    if let Some(pre) = option_env!("BORUIX_INIT_ARGS") {
        for want in pre.split_ascii_whitespace() {
            if want == "--selftest" { run_selftest = true; }
            else if want == "--selftest-quick" { run_selftest_quick = true; }
            else if let Some(rest) = want.strip_prefix("--run=") {
                // 构建期常量是 &'static str，可直接借出。
                run_cmd = Some(rest.as_bytes());
            }
        }
    }

    // 1. 欢迎信息（验证 write）。
    let _ = write(STDOUT, b"[init] Hello from real userspace (Rust + libsys)!\n");

    // 2. 查询内核版本号（验证 info）。
    match info(libsys::nr::INFO_VERSION) {
        Ok(ver) => print_hex(b"[init] kernel version", ver),
        Err(_) => {
            let _ = write(STDOUT, b"[init] info(version) failed\n");
        }
    }

    // 3. 查询堆断点（验证 brk）。
    match brk(0) {
        Ok(b) => print_hex(b"[init] heap break", b),
        Err(_) => {
            let _ = write(STDOUT, b"[init] brk(0) failed\n");
        }
    }

    // 4. 主动让出 CPU（验证 yield 原语；单进程下无可让出，立即返回）。
    match yield_now() {
        Ok(()) => {
            let _ = write(STDOUT, b"[init] yielded cpu\n");
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] yield failed\n");
        }
    }

    // 信号/libc 自检已移入 `selftest` 命令（用户要求开机直达 shell；按需运行）。

    // 4.0 数据盘内容自检：证明 disk.img 的文件**真能被读出**，而不只是挂上了。
    //
    // 挂载成功只说明 EXT2 超级块可解析；内容是否正确取决于 SDK 写入路径
    // （sdk/diskfiles -> mkimg -> disk.img）与内核读取路径是否真的对上。
    // 二者中间的任一处出错，"挂载成功"都会照样打印。故此处实读一个文件。
    //
    // 盘可能不存在（未挂 -drive），故失败只如实记录，不阻断启动。
    {
        let _ = write(STDOUT, b"[init] probing data disk contents...\n");
        let probe = "/volumes/BORUIX_DATA/welcome.txt";
        match libsys::read_to_end(probe) {
            Ok(bytes) => {
                let _ = write(STDOUT, b"[init] read ");
                let _ = write(STDOUT, probe.as_bytes());
                let _ = write(STDOUT, b" (");
                let mut nbuf = [0u8; 8];
                let _ = write(STDOUT, dec_u64(bytes.len() as u64, &mut nbuf));
                let _ = write(STDOUT, b" bytes), first line: ");
                // 只打印首行，避免刷屏；同时足以证明内容来自 diskfiles 而非硬编码。
                let end = bytes.iter().position(|&b| b == b'\n').unwrap_or(bytes.len());
                let _ = write(STDOUT, &bytes[..end]);
                let _ = write(STDOUT, b"\n");
            }
            Err(_) => {
                let _ = write(
                    STDOUT,
                    b"[init] data disk not present or /volumes/BORUIX_DATA/welcome.txt missing (non-fatal)\n",
                );
            }
        }
    }

    // 4.1 验证用户态 VFS 系统调用（M6.2: open/write/read/seek/readdir/mkdir/read_to_end）。
    let _ = write(STDOUT, b"[init] testing userspace VFS syscalls...\n");
    let _ = libsys::mkdir("/config/init_test", libsys::Permissions::all());
    match libsys::open(
        "/config/init_test/welcome.txt",
        libsys::OpenFlags::CREATE_OR_TRUNCATE,
        libsys::Permissions::all(),
    ) {
        Ok(fd) => {
            let _ = libsys::write(fd, b"BORUIX userspace VFS syscalls OK!");
            let _ = libsys::close(fd);
            match libsys::read_to_end("/config/init_test/welcome.txt") {
                Ok(bytes) => {
                    let _ = write(STDOUT, b"[init] read_to_end: ");
                    let _ = write(STDOUT, &bytes);
                    let _ = write(STDOUT, b"\n");
                }
                Err(_) => {
                    let _ = write(STDOUT, b"[init] read_to_end failed\n");
                }
            }
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] open failed\n");
        }
    }

    // 4.2 验证用户态 JSON 第一公民与特殊 VFS（M6.3: /system/info/cpu, /processes/list）。
    let _ = write(STDOUT, b"[init] reading /system/info/cpu JSON...\n");
    if let Ok(cpu_bytes) = libsys::read_to_end("/system/info/cpu") {
        let _ = write(STDOUT, b"[init] /system/info/cpu: ");
        let _ = write(STDOUT, &cpu_bytes);
    }
    let _ = write(STDOUT, b"[init] reading /processes/list JSON...\n");
    if let Ok(proc_bytes) = libsys::read_to_end("/processes/list") {
        let _ = write(STDOUT, b"[init] /processes/list: ");
        let _ = write(STDOUT, &proc_bytes);
    }

    // 4.2.5 A2 音频管道阻塞往返自检已移入 `selftest` 命令（开机直达 shell）。
    // 注意：`selftest` 里跑 A2 必须在 intel-hda attach **之前**才有消费者槽
    // 可用（槽位独占）；shell 里跑时 intel-hda 常驻占槽，A2 会如实报 EBUSY——
    // 这是真实约束，不是回归。

    // 4.3 拉起用户态卷管理守护进程 volumed（ADR-030 §决策1a / P2-1）。
    //    独立后台进程，经 VOLUME syscall + DEVICE 事件通道做卷自动挂载编排；
    //    不等待（守护进程自身永不退出）。启动失败不阻断 shell（非致命）。
    match libsys::exec_path("/programs/volumed.elf", &[]) {
        Ok(pid) => {
            let mut buf = [0u8; 8];
            let _ = write(STDOUT, b"[init] volumed started (pid ");
            let _ = write(STDOUT, dec_u64(pid, &mut buf));
            let _ = write(STDOUT, b")\n");
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] exec_path(volumed.elf) failed (non-fatal)\n");
        }
    }

    // 4.3.1 拉起运行时驱动自动装载守护 driverd（ADR-037 决策 4 / P2-1/P2-2）。
    //    独立后台进程：开机扫描 /modules 声明并 spawn 驱动认领设备、监听设备事件、
    //    驱动崩溃/退出后自动重拉。永不退出；启动失败不阻断 shell（非致命）。
    match libsys::exec_path("/programs/driverd.elf", &[]) {
        Ok(pid) => {
            let mut buf = [0u8; 8];
            let _ = write(STDOUT, b"[init] driverd started (pid ");
            let _ = write(STDOUT, dec_u64(pid, &mut buf));
            let _ = write(STDOUT, b")\n");
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] exec_path(driverd.elf) failed (non-fatal)\n");
        }
    }

    // 4.3.1.1 拉起用户态账户守护进程 userd（ADR-040 §2.8 / A1-8）。
    //    读 /config/users.json → 逐账户 mkdir /users/<name> + chown + chmod 0700
    //    + identity 投影；账户表缺失/为空时如实驻留等待（不伪造账户）。
    //    不等待（守护进程自身永不退出）；启动失败不阻断 shell（非致命，
    //    /users 为空目录仍是诚实状态——账户表可由用户态工具直接读取）。
    match libsys::exec_path("/programs/userd.elf", &[]) {
        Ok(pid) => {
            let mut buf = [0u8; 8];
            let _ = write(STDOUT, b"[init] userd started (pid ");
            let _ = write(STDOUT, dec_u64(pid, &mut buf));
            let _ = write(STDOUT, b")\n");
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] exec_path(userd.elf) failed (non-fatal)\n");
        }
    }

    // 4.3.2 / 4.3.3 intel-hda（ICH6 HD Audio）用户态驱动与其后置的 audiod —— 已移除。
    //
    // **移除理由（实测，2026-09）**：该驱动在**无 HDA 控制器**的启动配置下
    // （即不传 `-device intel-hda`）不会「自查无设备并干净退出」，而是卡在内核态
    // 永不返回：它以 pid 5 长期占据 `run.current`，而调度器的 IRQ0 tick 只在
    // **用户态**边界做 RR 轮转（`tick` 顶部 `if frame.cs & 3 != 3 { return }`），
    // 故一个卡在内核态的进程**永远不会被抢占**——pid 7（shell）与 pid 8（前台
    // 子进程）双双停在 `Ready` 却永不获调度。
    //
    // 实测取证（QEMU monitor 冻结采样 + 内核 tick 探针，两者独立一致）：
    //   - `cur=5` 在全部采样中恒定不变，而 `p7=Ready`、`p8=Ready` 反复出现；
    //   - 前台 `^C` 的 SIGINT **确实**到达了 0x03 并被 shell 消费、kill 也发出了
    //     （pending 位实测为 1），但信号派发只在「当前进程」上做，pid 8 永不为
    //     当前进程，故 pending 永远挂着——表现为提示符永不回来。
    //
    // 现象与既有文档记录一致：`pic.rs` / `irq_owner.rs` 记载 intel-hda 实测
    // 「整系统冻结于 `irq_restore+6`，IF=0，RIP 两秒纹丝不动」，与本次冻结现场
    // 抓到的 RIP/RFLAGS 完全吻合。
    //
    // 因此**删除**本步（而不是继续用 `--skip-hda` 绕过）：保留一个默认会把系统
    // 卡死的启动步骤，等于让每条默认路径都背负这条缺陷。`--skip-hda` 开关随之
    // 取消（它唯一的用途就是绕开这里）。待 HDA 驱动的内核态不返回问题被单独
    // 修复后，本步可连同 `--skip-hda` 一起恢复。
    //
    // 相关但**不受影响**的部分：`/devices/audio/dsp` 设备节点、`audiod` 与
    // `audiofile` 用户程序、以及 `selftest` 里的音频用例都仍在仓库中，只是不再
    // 由 init 在开机序列里拉起（无消费者时 audiod 的写入本就会被如实拒绝）。

    // 4.4.1 T1-8：端到端同进程双线程示例（threaddemo）。放风暴前执行并专候收尸，
    //    使风暴的 waitpid_any 不会误收 threaddemo 的僵尸。
    // threaddemo / chelldemo / pthreaddemo / pthread_syncdemo / pthread_bench
    // 已移入 `selftest` 命令（开机直达 shell，按需运行）。

    // A2 音频管道 e2e 已移至 4.2.5（**必须在 intel-hda 认领消费者槽之前**）。
    // 此处不再调用：消费者槽位独占，迟跑必然 EBUSY。详见该处说明。

    // 4.6 shell 路径执行自检（shell-path-exec 端到端验证）。
    //
    // **为何在 init 而不是 shell 内建里做**：本环境 shell 的 stdin 是 PS/2 键盘
    // （非串口），无法自动驱动输入行；而 shell 支持经 argv 接收一条命令行。
    // 故由 init 以不同 argv 反复拉起 shell —— 每条 argv 就是一次真实执行，
    // 走的路径与用户手敲完全相同（exec_line -> run_command -> exec_via_path）。
    // 与既有 threaddemo/audioe2e 的驱动手法一致（ADR-029）。
    //
    // **必须放在 4.5 的 SIGKILL 风暴之前**：那个风暴会拉起 spinburn 并长时间
    // 运行（实测 300 秒未结束），放在它之后本自检根本不会被执行到。
    // shell 路径自检 / audiofile 播放自检（含负例）/ 跨核 SIGKILL 风暴
    // 已移入 `selftest` 命令（开机直达 shell，按需运行）。

    // 4.7 `--selftest` 专用：非交互跑 `selftest thread`（ADR-038 U1 的端到端验收）。
    //
    // **为何需要这一条**：`selftest` 是 shell 命令（`shell/src/commands.rs`），而本
    // 环境 shell 的 stdin 是 PS/2 键盘——无法从串口日志自动驱动输入行。所以
    // `selftest thread` 里的 forkdemo 验收在**自动化构建**中永远不会被执行到，
    // 除非由 init 经 argv 拉起（这正是上面 4.6 说明的既有手法，ADR-029）。
    //
    // 用**独立**的 `--selftest` 而非复用 `--skip-hda`：当初挂在 skip-hda 下，是因为
    // selftest 只是「绕开 HDA 的 GPF 去看别的路径」的附带产物。那个 GPF 的根因
    // （syscall 入口帧缺 RPL=3，见 kernel `ad6222a`）**已经修复**，两者不再有因果
    // 关系——继续耦合会让「我要跑 selftest」被迫等价于「我不想要 HDA」，
    // 既误导使用者，也让 selftest 在带 HDA 的真实配置下无法被跑到。
    //
    // 正常启动路径（不带任何 flag）行为完全不变。
    if run_selftest {
        let _ = write(
            STDOUT,
            b"[init] running `selftest thread` (non-interactive; ADR-038 U1 forkdemo E2E)\n",
        );
        match libsys::exec_path("/programs/shell.elf", b"selftest thread") {
            Ok(pid) => {
                let mut buf = [0u8; 8];
                let _ = write(STDOUT, b"[init] selftest(thread) shell pid ");
                let _ = write(STDOUT, dec_u64(pid, &mut buf));
                let _ = write(STDOUT, b"\n");
                // 专候本 pid 收尸：不能让 supervisor 的 waitpid_any 误收它的僵尸，
                // 否则退出码（本验收的关键证据）会丢在别处。
                let mut spins: u32 = 0;
                loop {
                    match libsys::waitpid_any() {
                        Ok(wr) if wr.pid == pid => {
                            let mut b2 = [0u8; 8];
                            let _ = write(STDOUT, b"[init] selftest(thread) exited with code ");
                            let _ = write(STDOUT, dec_u64(wr.code as u64, &mut b2));
                            let _ = write(STDOUT, b" (0 = all assertions passed)\n");
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => {
                            spins += 1;
                            if spins > 400_000 {
                                let _ = write(
                                    STDOUT,
                                    b"[init] selftest(thread) reap timeout (non-fatal)\n",
                                );
                                break;
                            }
                            let _ = libsys::yield_now();
                        }
                    }
                }
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] exec_path(shell.elf, selftest thread) failed\n");
            }
        }
    }

    // 4.8 `--selftest-quick` 专用：非交互跑 `selftest quick`（A2-5 账户查询 E2E 入口）。
    //
    // **为何需要独立 flag**：`libccheck`/`pwde2e` 都是"按需运行"的用户态验收，
    // 而本环境 shell 的 stdin 是 PS/2 键盘——无法从串口日志自动驱动输入行。
    // 故与 4.7 同法，由 init 经 argv 拉起 shell 执行一条真实命令行（ADR-029 手法）。
    //
    // **为何不复用 `--selftest`**：那个 flag 的语义已被 ADR-038 U1 钉死为
    // "跑 selftest thread（forkdemo E2E）"。把 quick 塞进去会让"我要验账户查询"
    // 被迫等价于"我要跑线程组"，与 4.7 注释里反对的耦合是同一类错误。
    if run_selftest_quick {
        let _ = write(
            STDOUT,
            b"[init] running `selftest quick` (non-interactive; A2-5 pwd E2E)\n",
        );
        match libsys::exec_path("/programs/shell.elf", b"selftest quick") {
            Ok(pid) => {
                let mut buf = [0u8; 8];
                let _ = write(STDOUT, b"[init] selftest(quick) shell pid ");
                let _ = write(STDOUT, dec_u64(pid, &mut buf));
                let _ = write(STDOUT, b"\n");
                let mut spins: u32 = 0;
                loop {
                    match libsys::waitpid_any() {
                        Ok(wr) if wr.pid == pid => {
                            let mut b2 = [0u8; 8];
                            let _ = write(STDOUT, b"[init] selftest(quick) exited with code ");
                            let _ = write(STDOUT, dec_u64(wr.code as u64, &mut b2));
                            let _ = write(STDOUT, b" (0 = all assertions passed)\n");
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => {
                            spins += 1;
                            if spins > 400_000 {
                                let _ = write(STDOUT, b"[init] selftest(quick) reap timeout (non-fatal)\n");
                                break;
                            }
                            let _ = libsys::yield_now();
                        }
                    }
                }
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] exec_path(shell.elf, selftest quick) failed\n");
            }
        }
    }
    // 4.9 `--run=<命令行>`：以非交互 shell 执行**任意**一条命令行（通用验收入口）。
    //
    // **为何需要通用形态**：4.7/4.8 是两个用途固定的开关；而"跑 libccheck"、"跑某条
    // 内建命令做端到端核对"这类需求会持续出现。与其每来一个就加一个 flag（flag 数量
    // 无上界、且每个都要复制一遍收尸循环），不如提供一个**不新增机制**的通用入口：
    // 传入什么命令行就执行什么——走的仍是 shell 的非交互 argv 路径（ADR-029），
    // 与用户手敲完全同路。
    //
    // 优先级最低：--selftest / --selftest-quick 语义明确，若同时给出以它们为准。
    if let Some(cmd) = run_cmd {
        if !run_selftest && !run_selftest_quick {
            let _ = write(STDOUT, b"[init] running non-interactive command: ");
            let _ = write(STDOUT, cmd);
            let _ = write(STDOUT, b"\n");
            match libsys::exec_path("/programs/shell.elf", cmd) {
                Ok(pid) => {
                    let mut buf = [0u8; 8];
                    let _ = write(STDOUT, b"[init] run shell pid ");
                    let _ = write(STDOUT, dec_u64(pid, &mut buf));
                    let _ = write(STDOUT, b"\n");
                    let mut spins: u32 = 0;
                    loop {
                        match libsys::waitpid_any() {
                            Ok(wr) if wr.pid == pid => {
                                let mut b2 = [0u8; 8];
                                let _ = write(STDOUT, b"[init] run exited with code ");
                                let _ = write(STDOUT, dec_u64(wr.code as u64, &mut b2));
                                let _ = write(STDOUT, b" (0 = all assertions passed)\n");
                                break;
                            }
                            Ok(_) => continue,
                            Err(_) => {
                                spins += 1;
                                if spins > 400_000 {
                                    let _ = write(STDOUT, b"[init] run reap timeout (non-fatal)\n");
                                    break;
                                }
                                let _ = libsys::yield_now();
                            }
                        }
                    }
                }
                Err(_) => {
                    let _ = write(STDOUT, b"[init] exec_path(shell.elf, --run) failed\n");
                }
            }
        }
    }

    // 4.10 A2-7（ADR-041 §1.3）：以 `login` 做**认证关口**，通过后才进入交互 shell。
    //
    // 【为何在 init 里做这一手】内核 `exec_path` 语义是**继承**父进程身份，没有"以指定
    // 身份 spawn"的参数。而 `login` 必须是 uid 0 + **精确能力集**，否则读不到 root-only
    // 的 shadow 文件。故唯一可行路径是：init 先把自己的身份设为 login 所需的能力集，
    // 再 exec login —— login 自己在认证成功后降权到目标用户。
    //
    // 【能力集为何是这些值】**实测定案**（ADR-041 §1.2.3，kernel `2d48106` 的
    // `test_login_capability_set`），不是推断：
    //   - uid 0 是 shadow 文件**属主** ⇒ 以属主命中读取，**不需要**任何 DAC 覆盖能力；
    //   - **必须不含 CAP_OWNER**：实测 `CAP_OWNER` **完全绕过**策略求值，持它则口令
    //     文件形同虚设（`test_shadow_file_separation` 第 ③ 组）；
    //   - **必须不含 CAP_SYSTEM**：持它者可调 `identity_set` 变为 uid 0 间接读得 shadow；
    //   - **含 CAP_KILL**：使普通用户的信号被 A2-0 单点判定拒绝（ADR-040 §3.5.4 #15）。
    //
    // 【为何不用 `ProcessIdentity::system(1)`】它的 caps 是
    // `SYSTEM|DEVICE|MEMORY|KILL|OWNER`（`process.rs:300`）——**含 OWNER**。
    // 直接用它等于把口令文件敞开，是本 ADR 明令禁止的做法，故此处处处显式写出。
    //
    // 【id 位定义】与内核 `task::Caps` 对齐（ADR-040 §2.3，共 5 位，无保留位）：
    // SYSTEM=1<<0, DEVICE=1<<1, MEMORY=1<<2, KILL=1<<3, OWNER=1<<4。此处只用 KILL。
    //
    // 【非交互路径不受影响】`--selftest` / `--selftest-quick` / `--run=` 三个自动化入口
    // 在上面已各自完成并 return/静默，**不会**走到这里；正常启动路径的行为由本节改变
    // （原本直接 exec shell，现在先过 login）——这正是 A2-7 的目标，且已记入 ADR-041。
    {
        /// `Caps::KILL`（1<<3）——防普通用户信号（C4）。
        const LOGIN_CAPS_KILL: u32 = 1 << 3;
        /// `Caps::SYSTEM`（1<<0）——**降权所需**（C3）。
        /// **实测**（kernel `test_login_downgrade_path` 事实 1）：`{uid0,KILL}` 调
        /// `identity_set(1000,1000,0)` 返回 **EACCES**、uid 仍为 0。A2-1 的规则是
        /// "无 `CAP_SYSTEM` 则 uid 不得改变"（`syscall.rs:2999`），故**没有它就无法降权**。
        /// 注意它**只**影响 `identity_set`，**不影响文件策略**（事实 2：`{uid0,SYSTEM|KILL}`
        /// 与 `{uid0,KILL}` 读 shadow 结果相同），故不扩大 login 的读表能力。
        const LOGIN_CAPS_SYSTEM: u32 = 1 << 0;
        /// login 的完整能力集 = `SYSTEM | KILL`。**不含 OWNER**——含它则口令文件形同虚设。
        const LOGIN_CAPS: u32 = LOGIN_CAPS_SYSTEM | LOGIN_CAPS_KILL;
        let _ = write(STDOUT, b"[init] starting login (uid 0, caps=SYSTEM|KILL; no OWNER)\n");
        match libsys::identity_set(0, 0, LOGIN_CAPS) {
            Ok(()) => {}
            Err(_) => {
                // 身份设置失败：**不**降级继续（否则 login 读不到 shadow，
                // 或更糟——以 init 的 System 全能力身份启动 login，等于把口令文件敞开）。
                let _ = write(STDOUT, b"[init] identity_set for login FAILED; refusing to continue\n");
            }
        }
        // 认证失败 3 次后 login 以非零退出。此处**只登录一次**：退出后交由下面
        // 第 5 节的 supervisor 循环处理——而该循环的重生目标是**再次拉起 login**
        // （类 getty 语义），**绝不**直接拉起 shell。此前版本曾把 supervisor 的
        // 重生目标写成 shell，等于"认证未通过仍给了 shell"（实测：连续 6 次空
        // 回车 → 3 次尝试耗尽 → 直接得到 uid 0 的交互 shell，认证关口完全旁路），
        // 现已修复并记入 ADR-041 §1.3 / R13。
        match libsys::exec_path("/programs/login.elf", &[]) {
            Ok(pid) => {
                let mut buf = [0u8; 8];
                let _ = write(STDOUT, b"[init] login pid ");
                let _ = write(STDOUT, dec_u64(pid, &mut buf));
                let _ = write(STDOUT, b"\n");
                let mut spins: u32 = 0;
                loop {
                    match libsys::waitpid_any() {
                        Ok(wr) if wr.pid == pid => {
                            let mut b2 = [0u8; 8];
                            let _ = write(STDOUT, b"[init] login exited with code ");
                            let _ = write(STDOUT, dec_u64(wr.code as u64, &mut b2));
                            let _ = write(STDOUT, b"\n");
                            break;
                        }
                        Ok(_) => continue,
                        Err(_) => {
                            spins += 1;
                            if spins > 400_000 {
                                let _ = write(STDOUT, b"[init] login reap timeout\n");
                                break;
                            }
                            let _ = libsys::yield_now();
                        }
                    }
                }
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] exec_path(login.elf) failed\n");
            }
        }
        // **认证关口语义（关键）**：login 内部认证成功后，会在**自己的**进程里降权并
        // exec shell —— 也就是说成功的会话**不会**回到这里。因此走到本行即意味着
        // login 是失败退出（3 次机会耗尽）或无法启动。此时 init 的身份是上面设的
        // "uid 0 + SYSTEM|KILL"（login 所需），不带它去开 shell，而是由 supervisor
        // **重新拉起 login** 让认证重来——绝无"未认证 shell"。此处如实记录，
        // 避免把"登录失败"误读为"系统启动异常"。
        let _ = write(STDOUT, b"[init] login session ended; supervisor will restart login\n");
    }

    // 5. init 进入 supervisor 循环：拉起 login →（认证成功后 login 自行 exec shell）
    //    → 等会话退出 → 重生 login。类 getty/SysV 登录循环语义，PID 1 永不退出。
    //    也负责收尸被过继给 init 的孤儿进程，并区分日志。
    //
    // 【重生目标必须是 login，不能是 shell（R13，ADR-041 §1.3）】本循环若直接
    // exec shell，则任何未认证者只需让 login 失败退出（3 次空回车即耗尽）就能
    // 拿到交互 shell——且 supervisor 无 `identity_set`，shell 继承的是上面为
    // login 设的 "uid 0 + SYSTEM|KILL"：实测等于「6 次回车进 root shell」。
    // 修复后 supervisor 的重生目标只有 login；shell 的启动点唯一收敛在
    // login 内部（认证通过后 exec），init 从不直接开 shell。
    let _ = write(STDOUT, b"[init] entering supervisor loop\n");
    // supervisor：拉起 login 一次，然后**只等**；只有确认会话进程本身已退出才重生。
    //
    // 旧实现把 `exec_path(shell)` 放在 `loop` 顶部。收尸分支无论走哪条路都要
    // 回到循环顶再 exec 一遍 —— 于是**每收到一个被过继的孤儿就重开一个 shell**，
    // 而原 shell 仍活着。多个 shell 并发 `read(STDIN)` 同一个键盘环形缓冲，
    // 把一次击键瓜分给不同进程（实测 `ls` → `lls`/`s`/`l`，`clear` → `llear`，
    // `cat not-an-elf.txt` → `t not-an-f.tt`）—— 用户报的「命令对不对全靠运气」。
    //
    // 结构纪律：`exec` 属于**重生**动作，必须在等待循环**之外**；等待循环内
    // 只允许两类出口 —— 继续等（孤儿/瞬时失败）或跳出重生（会话真死了）。
    loop {
        // ---- 重生点：只有走到这里才拉起新 login（认证关口）----
        let session_pid: u64 = loop {
            match libsys::exec_path("/programs/login.elf", &[]) {
                Ok(pid) => {
                    let mut buf = [0u8; 8];
                    let _ = write(STDOUT, b"[init] login started (pid ");
                    let _ = write(STDOUT, dec_u64(pid, &mut buf));
                    let _ = write(STDOUT, b")\n");
                    break pid;
                }
                Err(_) => {
                    let _ = write(STDOUT, b"[init] exec_path(login.elf) failed, retrying...\n");
                    // 启动失败时短眠再试（避免忙转），走 TASK_WAIT(0, 500ms)
                    let _ = libsys::sleep(500_000_000);
                }
            }
        };
        // ---- 等待循环：绝不再 exec ----
        loop {
            match waitpid_any() {
                Ok(wr) => {
                    // 判断退出的是会话进程（login→shell）还是被过继的孤儿：读
                    // /processes/{session_pid}/status。可读 → 会话还在；
                    // NotFound → 会话已死，跳出本循环去重生 login。
                    let mut path_buf = [0u8; 32];
                    let prefix = b"/processes/";
                    let suffix = b"/status";
                    path_buf[..prefix.len()].copy_from_slice(prefix);
                    let mut pid_buf = [0u8; 8];
                    let pid_str = dec_u64(session_pid, &mut pid_buf);
                    let start = prefix.len();
                    path_buf[start..start + pid_str.len()].copy_from_slice(pid_str);
                    let end = start + pid_str.len();
                    path_buf[end..end + suffix.len()].copy_from_slice(suffix);
                    let path = core::str::from_utf8(&path_buf[..end + suffix.len()])
                        .unwrap_or("/processes/list");
                    if libsys::read_to_end(path).is_ok() {
                        // 会话进程仍在运行 → 退出的是被过继给 init 的孤儿。
                        // 继续等下一个，**绝不重生**。
                        let mut buf = [0u8; 8];
                        let _ = write(STDOUT, b"[init] reaped orphan (code ");
                        let _ = write(STDOUT, dec_u64(wr.code, &mut buf));
                        let _ = write(STDOUT, b"), continuing\n");
                    } else {
                        // 会话已退出（用户 shell 正常退出，或 login 认证失败）
                        // → 跳出等待循环，外层重生 login（重新认证，绝不开 shell）。
                        let mut buf = [0u8; 8];
                        let _ = write(STDOUT, b"[init] session ended (code ");
                        let _ = write(STDOUT, dec_u64(wr.code, &mut buf));
                        let _ = write(STDOUT, b"), respawning login\n");
                        break;
                    }
                }
                Err(_) => {
                    // 等待失败（含 `WouldBlock`：本核暂无就绪者但子进程仍在跑）。
                    // 短眠后继续等，**绝不重生** —— 那正是并发 shell 抢键盘的成因
                    // （此前忙转 `yield_now`，会话常驻等待时 init 空耗 CPU）。
                    let _ = libsys::sleep(100_000_000);
                }
            }
        }
    }
}

/// `libsys::Error` 的**稳定文本名**，用于诊断输出。
///
/// 为什么不直接 `{:?}`：`Debug` 的表示形式属于实现细节，且 `no_std` 下
/// 格式化路径体积可观。这里只映射本处实际可能遇到的几种，未知如实标 `Other`
/// —— 不猜、不伪造（S09）。
fn err_name(e: libsys::Error) -> &'static [u8] {
    match e {
        libsys::Error::NotFound => b"NotFound",
        libsys::Error::PermissionDenied => b"PermissionDenied",
        libsys::Error::InvalidParam => b"InvalidParam",
        libsys::Error::WouldBlock => b"WouldBlock",
        libsys::Error::NoSpace => b"NoSpace",
        libsys::Error::Io => b"Io",
        _ => b"Other",
    }
}

/// 把无符号整数格式化为十进制字节，写入 `buf`，返回有效长度。
///
/// **契约**：`buf` 至少 20 字节（`u64` 的十进制上界）。内部用 20 字节中转，
/// 写回时若 `buf` 偏小会按 `buf` 长度截断——调用方须按其值的量级选择缓冲。
fn dec_u64(v: u64, buf: &mut [u8; 8]) -> &[u8] {
    let mut tmp = [0u8; 20];
    let mut n = v;
    let mut i = 0;
    if n == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut j = 0;
    while i > 0 {
        i -= 1;
        buf[j] = tmp[i];
        j += 1;
    }
    &buf[..j]
}
