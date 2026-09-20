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
/// 等待音频消费者就位的有界重试轮数。
///
/// 取值依据：intel-hda 要完成"认领控制器 -> 复位 -> 枚举 codec -> R1 能力查询
/// -> attach -> 预填"才置位 consumer。单次 `yield_now()` 会让出整个调度轮，
/// 故几百轮足以覆盖，且不会像初版的 3000 那样拖上数分钟。
/// 有界是刻意的：无 HDA 设备时**必须**能退出并如实跳过（S20 失败模式优先）。
const AUDIO_ATTACH_WAIT_ROUNDS: u32 = 600;

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
    // 两个开关**相互独立**（曾经 selftest 挂在 skip-hda 下，那是因为 selftest 当时
    // 只是「绕开 HDA 的 GPF」的附带产物；GPF 根因修复后两者已无因果关系，
    // 继续耦合会让「跑 selftest」与「不启动 HDA」这两件不相干的事绑死）：
    //   --skip-hda      ：不拉起 intel-hda（诊断用：隔离显示/音频侧影响）
    //   --selftest      ：以非交互 shell 跑一遍 `selftest thread`（ADR-038 U1 入口）
//   --selftest-quick：以非交互 shell 跑一遍 `selftest quick`（A2-5 账户查询 E2E 入口）
    let mut skip_hda = false;
    let mut run_selftest = false;
    let mut run_selftest_quick = false;
    if argc > 0 && !argv.is_null() {
        let mut i = 0isize;
        while i < argc {
            // SAFETY: i < argc，argv 由内核 exec 路径按 C 数组构造，NUL 结尾。
            let p = unsafe { *argv.offset(i) };
            if !p.is_null() {
                if arg_eq(p, b"--skip-hda") { skip_hda = true; }
                else if arg_eq(p, b"--selftest") { run_selftest = true; }
                else if arg_eq(p, b"--selftest-quick") { run_selftest_quick = true; }
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
            else if want == "--skip-hda" { skip_hda = true; }
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

    // 4.3.2 拉起阶段三 intel-hda（ICH6 HD Audio）用户态声卡驱动带起。
    //    无 HDA 控制器（QEMU 未加 -device intel-hda）时驱动自查无设备并干净退出——
    //    非致命。加 -device intel-hda 后驱动认领/复位/枚举 codec（阶段三带起）。
    //
    // **可诊断性开关（ADR-038 U1 引入）**：以 argv 含 `--skip-hda` 启动 init 时跳过本步。
    // 动机是实测踩到的一个**与本开关无关的既有缺陷**：`br --release`（注意：**不带**
    // 任何 `--test-*`）时，intel-hda 在 `[uio] claim mapped ... -> user 0x101000000`
    // 之后立刻触发 General Protection Fault（vector 0xd，rip 0xffffffff8007614d），
    // 整个用户态启动因此停摆，永远到不了 shell。该缺陷在**暂存本工作全部改动后的
    // 纯净检出**上以**同一 rip** 复现，故与本工作无关，属另一条独立线索。
    //
    // 加这个开关而不是直接改崩溃点：声卡驱动的 MMIO 复位序列需要真实设备语义，
    // 不该为通过一项进程模型的验收而仓促改动；而本开关让**其它用户态路径**（含
    // forkdemo 端到端验收）能在该缺陷修复前继续被验证。跳过时**如实打印**，
    // 绝不静默（失败必须可见）。
    if !skip_hda {
    // `--quiet`：驱动本身默认会打 150 行 bring-up 取证（CORB/RIRB 轮询、
            // codec 枚举、放大路由），足够淹没 shell 提示符。此处显式要求只留
            // 结论行；需要逐步取证时去掉该参数单独跑驱动即可。
            match libsys::exec_path("/programs/intel-hda.elf", b"--quiet") {
        Ok(pid) => {
            let mut buf = [0u8; 8];
            let _ = write(STDOUT, b"[init] intel-hda started (pid ");
            let _ = write(STDOUT, dec_u64(pid, &mut buf));
            let _ = write(STDOUT, b")\n");
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] exec_path(intel-hda.elf) failed (non-fatal)\n");
        }
        }
    } else {
        let _ = write(
            STDOUT,
            b"[init] intel-hda SKIPPED (--skip-hda; see ADR-038 U1 note on the pre-existing GPF)\n",
        );
    }

    // 4.3.3 A3：拉起**音频流生产者**，把已知 PCM 写进 /devices/audio/dsp。
    //
    // **必须等 intel-hda 完成 attach()**：写入路径要求 ring 已有消费者，
    // 无消费者时写入是**如实拒绝**（A1 的设计，不静默丢弃）。
    //
    // 【修正】初版这里写的是 `for _ in 0..3000 { yield_now() }` 作为"等一会儿"。
    // 实测证明那是**错的**：`yield_now()` 是一次完整的调度往返，3000 次要跑
    // 好几分钟（每次都要让给 volumed/driverd 等所有就绪线程），实测日志里
    // 出现 16927 行 yield syscall、生产者迟迟不启动，A3 流式几乎没跑起来。
    //
    // 更根本的问题是：**延时不是同步**。它既不保证 intel-hda 已经 attach，
    // 也不在它 attach 后立即继续——纯属猜一个数字（S13），且不可验证（S20）。
    //
    // 现在改为**观测真实前置条件**：轮询 `/devices/audio/dsp/status` 的
    // `attached` 字段（A1 已如实披露该状态，S15 单一事实源），成立即派生。
    // 有界重试：无 HDA 设备时 intel-hda 会干净退出，此时**如实报告并跳过**
    // 生产者，而不是派生一个注定失败的进程。
    let mut attached = false;
    for _ in 0..AUDIO_ATTACH_WAIT_ROUNDS {
        if let Ok(st) = libsys::read_to_end("/devices/audio/dsp/status") {
            // 只做最朴素的子串判定：JSON 里 "attached":true 即表示消费者已就位。
            if contains(&st, b"\"attached\":true") {
                attached = true;
                break;
            }
        }
        let _ = libsys::yield_now();
    }
    // 记录我们**实际观测到**的状态，而不是假定的状态（S09）。
    let _ = write(STDOUT, b"[init] audio ring consumer attached = ");
    let _ = write(STDOUT, if attached { b"true\n" } else { b"false\n" });

    // ---- 4.3.3.1 拉起混音守护进程 audiod（守护进程，保留常驻）----
    //
    // **audiod 是 dsp 的写者，不是消费者，故不调 AUDIO_ATTACH**（实测修正，
    // 见 audiod/src/main.rs 顶部说明）。它只需 dsp 上**已有**消费者——
    // 即 intel-hda 已 attach 占位。
    //
    // 顺序约束：audiod 必须排在 intel-hda **之后**（否则 dsp 无人消费，
    // audiod 写入会被如实拒绝）。
    //
    // 原 8 MiB x2 路 stream 生产者与 fpcheck 已移入 `selftest` 命令：
    // 它们是**有界的测试负载**（2 x 44s 实时播放 + FP 演示），不是服务——
    // 开机必跑会让 shell 迟到两三分钟（用户实测）。audiod 只 attach/待命，
    // 不产生声音，留在开机序列里没有代价。
    if !attached {
        // 如实说明：没有消费者时 audiod 的写入会被拒，跳过派生（honest skip）。
        let _ = write(
            STDOUT,
            b"[init] no audio consumer (no HDA device?); skipping audiod (honest skip)\n",
        );
    } else {
        match libsys::exec_path("/programs/audiod.elf", b"") {
            Ok(pid) => {
                let mut buf = [0u8; 8];
                let _ = write(STDOUT, b"[init] audiod started (pid ");
                let _ = write(STDOUT, dec_u64(pid, &mut buf));
                let _ = write(STDOUT, b")\n");
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] exec_path(audiod.elf) failed (non-fatal)\n");
            }
        }
    }

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

    // 5. init 进入 supervisor 循环：拉起 shell → 等其退出 → 重生。
    //    类 SysV 登录循环语义，PID 1 永不退出。
    //    也负责收尸被过继给 init 的孤儿进程，并区分日志。
    let _ = write(STDOUT, b"[init] entering supervisor loop\n");
    // supervisor：拉起 shell 一次，然后**只等**；只有确认 shell 本身已退出才重生。
    //
    // 旧实现把 `exec_path(shell)` 放在 `loop` 顶部。收尸分支无论走哪条路都要
    // 回到循环顶再 exec 一遍 —— 于是**每收到一个被过继的孤儿就重开一个 shell**，
    // 而原 shell 仍活着。多个 shell 并发 `read(STDIN)` 同一个键盘环形缓冲，
    // 把一次击键瓜分给不同进程（实测 `ls` → `lls`/`s`/`l`，`clear` → `llear`，
    // `cat not-an-elf.txt` → `t not-an-f.tt`）—— 用户报的「命令对不对全靠运气」。
    //
    // 结构纪律：`exec` 属于**重生**动作，必须在等待循环**之外**；等待循环内
    // 只允许两类出口 —— 继续等（孤儿/瞬时失败）或跳出重生（shell 真死了）。
    loop {
        // ---- 重生点：只有走到这里才拉起新 shell ----
        let shell_pid: u64 = loop {
            match libsys::exec_path("/programs/shell.elf", &[]) {
                Ok(pid) => {
                    let mut buf = [0u8; 8];
                    let _ = write(STDOUT, b"[init] shell started (pid ");
                    let _ = write(STDOUT, dec_u64(pid, &mut buf));
                    let _ = write(STDOUT, b")\n");
                    break pid;
                }
                Err(_) => {
                    let _ = write(STDOUT, b"[init] exec_path(shell.elf) failed, retrying...\n");
                    // 启动失败时短眠再试（避免忙转），走 TASK_WAIT(0, 500ms)
                    let _ = libsys::sleep(500_000_000);
                }
            }
        };
        // ---- 等待循环：绝不再 exec ----
        loop {
            match waitpid_any() {
                Ok(wr) => {
                    // 判断退出的是 shell 还是被过继的孤儿：读
                    // /processes/{shell_pid}/status。可读 → shell 还在；
                    // NotFound → shell 已死，跳出本循环去重生。
                    let mut path_buf = [0u8; 32];
                    let prefix = b"/processes/";
                    let suffix = b"/status";
                    path_buf[..prefix.len()].copy_from_slice(prefix);
                    let mut pid_buf = [0u8; 8];
                    let pid_str = dec_u64(shell_pid, &mut pid_buf);
                    let start = prefix.len();
                    path_buf[start..start + pid_str.len()].copy_from_slice(pid_str);
                    let end = start + pid_str.len();
                    path_buf[end..end + suffix.len()].copy_from_slice(suffix);
                    let path = core::str::from_utf8(&path_buf[..end + suffix.len()])
                        .unwrap_or("/processes/list");
                    if libsys::read_to_end(path).is_ok() {
                        // shell 仍在运行 → 退出的是被过继给 init 的孤儿。
                        // 继续等下一个，**绝不重生**。
                        let mut buf = [0u8; 8];
                        let _ = write(STDOUT, b"[init] reaped orphan (code ");
                        let _ = write(STDOUT, dec_u64(wr.code, &mut buf));
                        let _ = write(STDOUT, b"), continuing\n");
                    } else {
                        // shell 已退出 → 跳出等待循环，外层重生。
                        let mut buf = [0u8; 8];
                        let _ = write(STDOUT, b"[init] shell exited (code ");
                        let _ = write(STDOUT, dec_u64(wr.code, &mut buf));
                        let _ = write(STDOUT, b"), respawning\n");
                        break;
                    }
                }
                Err(_) => {
                    // 等待失败（含 `WouldBlock`：本核暂无就绪者但子进程仍在跑）。
                    // 让出后继续等，**绝不重生** —— 那正是并发 shell 抢键盘的成因。
                    let _ = libsys::yield_now();
                }
            }
        }
    }
}

/// 把无符号整数格式化为十进制字节，写入 `buf`，返回有效长度。
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
