//! BORUIX PID 1：首个用户态进程（init）。
//!
//! 依赖 libsys 的薄封装调用 syscall。作为 M4.4 的真实用户程序，验证
//! 「Rust no_std 用户程序 → 编译成 ELF → 内核加载运行」完整链路。
//!
//! 入口约定：libsys 提供 `_start`，调用本文件导出的 `user_main`，
//! 其返回值作为进程退出码。

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};
use libsys::{brk, exec_path, info, kill, waitpid_any, write, yield_now, STDOUT};

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

// ---- ADR-034 S1-14：信号端到端自测（libsys action/raise + 用户 handler + sigreturn） ----

/// 用户 SIGUSR1 handler 置位标记（供主流程验证 handler 确实被投递并 sigreturn 恢复）。
static SIG_HANDLER_RAN: AtomicU32 = AtomicU32::new(0);

/// naked handler：置位 `SIG_HANDLER_RAN` 后 `ret` → restorer → rt_sigreturn。
/// 内核 `deliver_handler` 把 handler 返回地址写成 restorer 地址，`ret` 即跳 restorer。
#[unsafe(naked)]
unsafe extern "C" fn init_sigusr1_handler() {
    core::arch::naked_asm!(
        "mov dword ptr [rip + {ran}], 1",
        "ret",
        ran = sym SIG_HANDLER_RAN,
    );
}

/// S1-14 自测：注册 SIGUSR1 handler → 向自身 raise → 返回用户态时投递进 handler
/// → restorer → rt_sigreturn 恢复。任何失败打印错误但不中断后续启动（防御式）。
fn signal_selftest() {
    let _ = write(STDOUT, b"[init] signal: testing S1-14 action/raise/handler...\n");
    let handler_addr = init_sigusr1_handler as *const () as usize as u64;
    if let Err(_) = libsys::signal::action(libsys::signal::SIGUSR1, handler_addr, 0) {
        let _ = write(STDOUT, b"[init] signal: action(SIGUSR1) failed\n");
        return;
    }
    let _ = write(STDOUT, b"[init] signal: action(SIGUSR1, handler) ok\n");
    // 向自身（init 恒为 PID 1）raise SIGUSR1。
    match libsys::signal::raise(1, libsys::signal::SIGUSR1) {
        Ok(_) => {}
        Err(_) => {
            let _ = write(STDOUT, b"[init] signal: raise(SIGUSR1) failed\n");
            return;
        }
    }
    let _ = write(STDOUT, b"[init] signal: raise ok, awaiting delivery...\n");
    // 让出触发返回用户态投递（若 raise 返回时未投递，yield 再给一次机会）。
    let _ = yield_now();
    let _ = yield_now();
    if SIG_HANDLER_RAN.load(Ordering::SeqCst) == 1 {
        let _ = write(STDOUT, b"[init] signal: SIGUSR1 handler ran + sigreturn ok (S1-14 PASS)\n");
    } else {
        let _ = write(STDOUT, b"[init] signal: handler did NOT run (S1-14 FAIL)\n");
    }
}


/// libc 最小链路自检（ADR 目标：内核→libsys→libc→init 在开机即通）。
///
/// 验证 libc 的核心 C ABI（malloc/string/printf/strtol/time），打印逐项
/// OK/FAIL 与汇总。防御式：失败仅记录，不中断启动流程。
fn libc_selftest() {
    let _ = write(STDOUT, b"[init] libc: testing core C ABI...\n");
    let mut pass = 0u32;
    let mut fail = 0u32;

    // 1) malloc/free 堆分配。
    unsafe {
        let p = libc::malloc::malloc(48);
        if !p.is_null() {
            *p.add(0) = 0x42;
            *p.add(47) = 0x43;
            if p.add(0).read() == 0x42 && p.add(47).read() == 0x43 {
                pass += 1;
                let _ = write(STDOUT, b"[init] libc: malloc/free OK\n");
            } else {
                fail += 1;
                let _ = write(STDOUT, b"[init] libc: malloc writable FAIL\n");
            }
            libc::malloc::free(p);
        } else {
            fail += 1;
            let _ = write(STDOUT, b"[init] libc: malloc FAIL\n");
        }
    }

    // 2) string：strlen/strcmp。
    unsafe {
        let a = b"hello\0".as_ptr() as *const i8;
        if libc::string::strlen(a) == 5 && libc::string::strcmp(a, b"hello\0".as_ptr() as *const i8) == 0 {
            pass += 1;
            let _ = write(STDOUT, b"[init] libc: string OK\n");
        } else {
            fail += 1;
            let _ = write(STDOUT, b"[init] libc: string FAIL\n");
        }
    }

    // 3) snprintf（格式引擎 + 浮点）。
    unsafe {
        let mut buf = [0u8; 64];
        let n = libc::stdio::snprintf(
            buf.as_mut_ptr() as *mut i8, buf.len(),
            b"v=%d f=%.2f\0".as_ptr() as *const i8, 7, 3.14,
        );
        // 期望 "v=7 f=3.14"（长度 10）。
        if n == 10 {
            pass += 1;
            let _ = write(STDOUT, b"[init] libc: snprintf OK\n");
        } else {
            fail += 1;
            let _ = write(STDOUT, b"[init] libc: snprintf FAIL\n");
        }
    }

    // 4) strtol 整数解析。
    unsafe {
        if libc::stdlib::strtol(b"-99\0".as_ptr() as *const i8, core::ptr::null_mut(), 10) == -99 {
            pass += 1;
            let _ = write(STDOUT, b"[init] libc: strtol OK\n");
        } else {
            fail += 1;
            let _ = write(STDOUT, b"[init] libc: strtol FAIL\n");
        }
    }

    // 5) time 墙钟读数。
    {
        if libc::time::time(core::ptr::null_mut()) > 0 {
            pass += 1;
            let _ = write(STDOUT, b"[init] libc: time OK\n");
        } else {
            fail += 1;
            let _ = write(STDOUT, b"[init] libc: time FAIL\n");
        }
    }

    // 汇总。
    let _ = write(STDOUT, b"[init] libc: selftest passed=");
    let mut b1 = [0u8; 8];
    let pb = u64_to_dec(pass as u64, &mut b1);
    let _ = write(STDOUT, pb);
    let _ = write(STDOUT, b" failed=");
    let mut b2 = [0u8; 8];
    let fb = u64_to_dec(fail as u64, &mut b2);
    let _ = write(STDOUT, fb);
    let _ = write(STDOUT, b"\n");
}

/// 把 u64 写成十进制字节（最小，无前导零）。
fn u64_to_dec(mut v: u64, buf: &mut [u8; 8]) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &buf[i..]
}

/// T1-8：拉起 threaddemo（端到端同进程双线程示例）并等待其完成、收尸。
///
/// 真实用户程序 /programs/threaddemo.elf 在自身进程内 thread_spawn 两个线程（共享
/// 组长 Arc 地址空间、各自独立 mmap 用户栈）→ 各自打印 → thread_exit → 组长 join。
/// init 以 exec_path 派生它并等 waitpid_any 收尸到其 pid。线程demo 秒级完成，故本
/// 阶段其它长驻子进程（volumed/fpcheck）不会先退出。
///
/// SMP 语义：waitpid_any 在"本核此刻无其它就绪进程可接盘、无法阻塞"时会返回
/// Err(WouldBlock/NotFound)——这**不是** threaddemo 已退/不存在，只是当前无法阻塞
/// 等待。故用「yield 让出 + 重试 waitpid_any」轮询直到真正收尸到 threaddemo 的 pid：
/// threaddemo 完成后留 zombie，随后的 waitpid_any 必能同步收尸。**绝不**在未收尸
/// threaddemo 前就放行进入下一阶段（跨核风暴），以免风暴的跨核 SIGKILL 与仍在跑的
/// threaddemo 线程并发触发调度竞争。
fn threaddemo_launch() {
    let _ = write(STDOUT, b"[init] launching threaddemo (T1-8 two-thread demo)\n");
    let pid = match libsys::exec_path("/programs/threaddemo.elf", &[]) {
        Ok(p) => p,
        Err(_) => {
            let _ = write(STDOUT, b"[init] exec_path(threaddemo.elf) failed (non-fatal)\n");
            return;
        }
    };
    let mut bbuf = [0u8; 8];
    let _ = write(STDOUT, b"[init] threaddemo spawned (pid ");
    let _ = write(STDOUT, dec_u64(pid, &mut bbuf));
    let _ = write(STDOUT, b"), waiting for it to join both threads...\n");
    // yield + waitpid_any 轮询，直到收尸 threaddemo 本体。Err（WouldBlock/NotFound）
    // 表示当前核心此刻无法阻塞等待（非 threaddemo 已死），让出再试；上限 20000 次
    // yield（约数秒）后仍未收到则记录并放行（防御式，理论上不达）。若意外收尸到
    // volumed/fpcheck 等非 threaddemo 子进程，记录后继续等 threaddemo。
    let mut spins: u32 = 0;
    loop {
        match waitpid_any() {
            Ok(wr) if wr.pid == pid => {
                let _ = write(STDOUT, b"[init] threaddemo reaped (code ");
                let _ = write(STDOUT, dec_u64(wr.code, &mut bbuf));
                let _ = write(STDOUT, b")\n");
                break;
            }
            Ok(wr) => {
                let _ = write(STDOUT, b"[init] waitpid_any reaped other child pid=");
                let _ = write(STDOUT, dec_u64(wr.pid, &mut bbuf));
                let _ = write(STDOUT, b" (continuing)\n");
            }
            Err(_) => {
                spins += 1;
                if spins > 20000 {
                    let _ = write(STDOUT, b"[init] threaddemo reap timeout (giving up)\n");
                    return;
                }
                let _ = yield_now();
            }
        }
    }
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

fn audio_e2e_launch() {
    let _ = write(STDOUT, b"[init] launching audioe2e (A2 blocking round-trip)\n");

    // ---- 1. 先派生 consumer 并让它跑起来（attach + 在空 ring 上阻塞）----
    let consumer = match libsys::exec_path("/programs/audioe2e.elf", b"consumer") {
        Ok(p) => p,
        Err(_) => {
            let _ = write(STDOUT, b"[init] audioe2e spawn failed (non-fatal)\n");
            return;
        }
    };
    let mut bbuf = [0u8; 8];
    let _ = write(STDOUT, b"[init] audioe2e consumer pid ");
    let _ = write(STDOUT, dec_u64(consumer, &mut bbuf));
    let _ = write(STDOUT, b"\n");
    // 让 consumer 跑到 fetch 并入睡。yield 保持 init 就绪（它若也阻塞，
    // 无就绪同伴可切，consumer 的阻塞路径就走不到）。
    for _ in 0..4000 {
        let _ = yield_now();
    }

    // ---- 2. init 经 VFS syscall 写入一帧 PCM，唤醒阻塞的 consumer ----
    // 填充必须与 audioe2e 的 consumer 端逐字节一致。
    const FRAME: usize = 256;
    let mut frame = [0u8; FRAME];
    let mut i = 0usize;
    while i < FRAME {
        frame[i] = ((i * 37) ^ (i >> 3)) as u8;
        i += 1;
    }
    let fd = match libsys::open(
        "/devices/audio/dsp",
        libsys::OpenFlags::READ_WRITE,
        libsys::Permissions::read_write(),
    ) {
        Ok(f) => f,
        Err(_) => {
            let _ = write(STDOUT, b"[init] audioe2e open dsp failed (non-fatal)\n");
            return;
        }
    };
    match write(fd, &frame) {
        Ok(n) if n == FRAME => {
            let _ = write(
                STDOUT,
                b"[init] audioe2e wrote 256B frame (should have woken blocked reader)\n",
            );
        }
        Ok(n) => {
            let _ = write(STDOUT, b"[init] audioe2e SHORT write ");
            let _ = write(STDOUT, dec_u64(n as u64, &mut bbuf));
            let _ = write(STDOUT, b" (expected 256) - FAIL\n");
        }
        Err(_) => {
            let _ = write(
                STDOUT,
                b"[init] audioe2e write FAILED (consumer not attached?)\n",
            );
        }
    }
    // 让被唤醒的 consumer 跑完校验/commit/detach。
    for _ in 0..4000 {
        let _ = yield_now();
    }

    // ---- 3. 收 consumer 退出码，断言 0 ----
    let mut spins: u32 = 0;
    loop {
        match waitpid_any() {
            Ok(wr) if wr.pid == consumer => {
                if wr.code == 0 {
                    let _ = write(
                        STDOUT,
                        b"[init] audioe2e PASS: blocked reader woke, verified, committed\n",
                    );
                } else {
                    let _ = write(STDOUT, b"[init] audioe2e FAIL: consumer exit=");
                    let _ = write(STDOUT, dec_u64(wr.code, &mut bbuf));
                    let _ = write(STDOUT, b"\n");
                }
                break;
            }
            // 收到别的子进程（volumed/fpcheck 等），记录后继续等 consumer。
            Ok(wr) => {
                let _ = write(STDOUT, b"[init] audioe2e reaped other pid=");
                let _ = write(STDOUT, dec_u64(wr.pid, &mut bbuf));
                let _ = write(STDOUT, b" (continuing)\n");
            }
            Err(_) => {
                spins += 1;
                if spins > 20000 {
                    let _ = write(STDOUT, b"[init] audioe2e reap timeout (giving up)\n");
                    return;
                }
                let _ = yield_now();
            }
        }
    }
}

/// T2-0：拉起 chelldemo（第一个真实 freestanding C 程序，x86-64 clang/lld 交叉链 +
/// crt0 + 直连 syscall，不依赖 Rust libc）并收尸，验证 C 运行时地基端到端。
/// chelldemo 立即打印并 exit(0)，故快速 poll 收尸即可；失败非致命。
fn chelldemo_launch() {
    let _ = write(STDOUT, b"[init] launching chelldemo (T2-0 C runtime, freestanding clang)\n");
    let pid = match libsys::exec_path("/programs/chelldemo.elf", &[]) {
        Ok(p) => p,
        Err(_) => { let _ = write(STDOUT, b"[init] exec_path(chelldemo.elf) failed (non-fatal)\n"); return; }
    };
    let mut bbuf = [0u8; 8];
    let _ = write(STDOUT, b"[init] chelldemo spawned (pid ");
    let _ = write(STDOUT, dec_u64(pid, &mut bbuf));
    let _ = write(STDOUT, b")\n");
    let mut spins: u32 = 0;
    loop {
        match waitpid_any() {
            Ok(wr) if wr.pid == pid => {
                let _ = write(STDOUT, b"[init] chelldemo reaped (code ");
                let _ = write(STDOUT, dec_u64(wr.code, &mut bbuf));
                let _ = write(STDOUT, b")\n");
                break;
            }
            Ok(_) => {}
            Err(_) => {
                spins += 1;
                if spins > 10000 { let _ = write(STDOUT, b"[init] chelldemo reap timeout\n"); return; }
                let _ = yield_now();
            }
        }
    }
}
/// T2-3：拉起 pthreaddemo（C pthread 生命周期端到端：create/join/detach/self）并收尸。
/// 快速执行并 exit(0)；失败非致命。
fn pthreaddemo_launch() {
    let _ = write(STDOUT, b"[init] launching pthreaddemo (T2-3 C pthread lifecycle)\n");
    let pid = match libsys::exec_path("/programs/pthreaddemo.elf", &[]) {
        Ok(p) => p,
        Err(_) => { let _ = write(STDOUT, b"[init] exec_path(pthreaddemo.elf) failed (non-fatal)\n"); return; }
    };
    let mut bbuf = [0u8; 8];
    let _ = write(STDOUT, b"[init] pthreaddemo spawned (pid ");
    let _ = write(STDOUT, dec_u64(pid, &mut bbuf));
    let _ = write(STDOUT, b")\n");
    let mut spins: u32 = 0;
    loop {
        match waitpid_any() {
            Ok(wr) if wr.pid == pid => {
                let _ = write(STDOUT, b"[init] pthreaddemo reaped (code ");
                let _ = write(STDOUT, dec_u64(wr.code, &mut bbuf));
                let _ = write(STDOUT, b")\n");
                break;
            }
            Ok(_) => {}
            Err(_) => {
                spins += 1;
                if spins > 20000 { let _ = write(STDOUT, b"[init] pthreaddemo reap timeout\n"); return; }
                let _ = yield_now();
            }
        }
    }
}
/// T2-4：拉起 pthread_syncdemo（C pthread 互斥/condvar/信号量端到端）并收尸。
/// 快速执行并 exit(0)；失败非致命。
fn pthread_syncdemo_launch() {
    let _ = write(STDOUT, b"[init] launching pthread_syncdemo (T2-4 C mutex/cond/sem)\n");
    let pid = match libsys::exec_path("/programs/pthread_syncdemo.elf", &[]) {
        Ok(p) => p,
        Err(_) => { let _ = write(STDOUT, b"[init] exec_path(pthread_syncdemo.elf) failed (non-fatal)\n"); return; }
    };
    let mut bbuf = [0u8; 8];
    let _ = write(STDOUT, b"[init] pthread_syncdemo spawned (pid ");
    let _ = write(STDOUT, dec_u64(pid, &mut bbuf));
    let _ = write(STDOUT, b")\n");
    let mut spins: u32 = 0;
    loop {
        match waitpid_any() {
            Ok(wr) if wr.pid == pid => {
                let _ = write(STDOUT, b"[init] pthread_syncdemo reaped (code ");
                let _ = write(STDOUT, dec_u64(wr.code, &mut bbuf));
                let _ = write(STDOUT, b")\n");
                break;
            }
            Ok(_) => {}
            Err(_) => {
                spins += 1;
                if spins > 40000 { let _ = write(STDOUT, b"[init] pthread_syncdemo reap timeout\n"); return; }
                let _ = yield_now();
            }
        }
    }
}
/// 通用 C 程序拉起 + 收尸：exec_path + waitpid_any 轮询，超时容忍。
fn launch_c_prog(path: &str, tag: &str) {
    let mut msg = [0u8; 96];
    let mut n = 0;
    for b in b"[init] launching ".iter() { msg[n] = *b; n += 1; }
    for b in tag.bytes() { msg[n] = b; n += 1; }
    for b in b"\n".iter() { msg[n] = *b; n += 1; }
    let _ = write(STDOUT, &msg[..n]);
    let pid = match libsys::exec_path(path, &[]) {
        Ok(pp) => pp,
        Err(_) => {
            let _ = write(STDOUT, b"[init] exec_path failed (non-fatal)\n");
            return;
        }
    };
    let mut spins: u32 = 0;
    loop {
        match waitpid_any() {
            Ok(wr) if wr.pid == pid => {
                let mut rp = [0u8; 8];
                let _ = write(STDOUT, b"[init] ");
                let _ = write(STDOUT, tag.as_bytes());
                let _ = write(STDOUT, b" reaped (code ");
                let _ = write(STDOUT, dec_u64(wr.code, &mut rp));
                let _ = write(STDOUT, b")\n");
                return;
            }
            Ok(_) => {}
            Err(_) => {
                spins += 1;
                if spins > 80000 { let _ = write(STDOUT, b"[init] reap timeout\n"); return; }
                let _ = yield_now();
            }
        }
    }
}

/// 跨核 spawn + SIGKILL terminate 风暴（S1 迁移 + 既有跨核终止 bug 的复现/回归脚手架）。
///
/// 每轮派生 W 个 spinburn 长驻子进程（least-loaded 分到各核），BSP 对其逐 kill(SIGKILL)，
/// 再 waitpid_any 收尸。复现: 跨核 SIGKILL '运行中/仅存其核' 的进程后, 被杀进程未被
/// 及时切走/可收尸 => 系统冻结。修复后此风暴应能多轮全绿(spawn==killed==reaped)。
fn cross_core_sigkill_storm() {
    const W: u32 = 5;
    const ROUNDS: u32 = 4;
    let _ = write(STDOUT, b"[init] cross-core SIGKILL storm: start\n");
    let mut rbuf = [0u8; 8];
    for round in 0..ROUNDS {
        let mut pids = [0u64; 8];
        let mut n = 0u32;
        for _ in 0..W {
            if let Ok(p) = exec_path("/programs/spinburn.elf", &[]) {
                if (n as usize) < pids.len() { pids[n as usize] = p; n += 1; }
            }
            for _ in 0..100 { let _ = yield_now(); }
        }
        if n == 0 { continue; }
        for _ in 0..2500 { let _ = yield_now(); }
        let mut killed = 0u32;
        for i in 0..n { if kill(pids[i as usize], 9).is_ok() { killed += 1; } }
        let mut reaped = 0u32;
        for _ in 0..n {
            match waitpid_any() { Ok(_) => { reaped += 1; } Err(_) => { break; } }
        }
        let _ = write(STDOUT, b"[init] storm round ");
        let _ = write(STDOUT, dec_u64(round as u64, &mut rbuf));
        let _ = write(STDOUT, b": spawn="); let _ = write(STDOUT, dec_u64(n as u64, &mut rbuf));
        let _ = write(STDOUT, b" killed="); let _ = write(STDOUT, dec_u64(killed as u64, &mut rbuf));
        let _ = write(STDOUT, b" reaped="); let _ = write(STDOUT, dec_u64(reaped as u64, &mut rbuf));
        let _ = write(STDOUT, b"\n");
    }
    let _ = write(STDOUT, b"[init] cross-core SIGKILL storm done\n");
}

/// init 主流程：打印信息、查询内核版本与堆断点、退出。
#[unsafe(no_mangle)]
pub extern "C" fn user_main(_argc: isize, _argv: *const *const u8) -> i32 {
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

    // 4.2 ADR-034 S1-14：信号端到端自测（防御式，失败不中断启动）。
    signal_selftest();

    // 4.2.1 libc 最小链路自检（内核→libsys→libc→init 开机即通；防御式）。
    libc_selftest();

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

    // 4.2.5 A2：音频管道端到端阻塞往返（plan_audio_vfs.md 批次二）。
    //
    // **为什么必须放在 intel-hda 之前**（A3 实测发现的顺序约束）：
    // A2 的测试方式是"一个进程 attach 成消费者并阻塞读取，另一个写一帧唤醒
    // 它"。而 attach 的消费者槽位是**独占**的；intel-hda 在 A3 起流时也会
    // attach 成消费者并**常驻不退**。若 A2 在 intel-hda 之后跑，它会拿到
    // EBUSY 而失败——A3 初版正是如此（实测 `attach (consumer) rejected`，
    // 随后 `audioe2e FAIL: consumer exit=1`）。
    //
    // 放在这里，两个测试**都**保持有效：先验证内核管道的阻塞/唤醒契约，
    // 再由 A3 的流式栈接管消费者身份。二者语义不同，不该互相遮蔽。
    audio_e2e_launch();

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

    // 4.3.2 拉起阶段三 intel-hda（ICH6 HD Audio）用户态声卡驱动带起。
    //    无 HDA 控制器（QEMU 未加 -device intel-hda）时驱动自查无设备并干净退出——
    //    非致命。加 -device intel-hda 后驱动认领/复位/枚举 codec（阶段三带起）。
    match libsys::exec_path("/programs/intel-hda.elf", &[]) {
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

    if !attached {
        // 如实说明为何不派生：没有消费者，生产者写了也会被拒。
        let _ = write(
            STDOUT,
            b"[init] no audio consumer (no HDA device?); skipping stream producer (honest skip)\n",
        );
    } else {
        // 生产者写的是**确定性** pattern：这是 A3 数据通路验收的前提。
        // 若数据不确定，判据就只能退化成主观的"听起来有声音"。
        match libsys::exec_path("/programs/audioe2e.elf", b"stream") {
            Ok(pid) => {
                let mut buf = [0u8; 8];
                let _ = write(STDOUT, b"[init] audio stream producer started (pid ");
                let _ = write(STDOUT, dec_u64(pid, &mut buf));
                let _ = write(STDOUT, b")\n");
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] exec_path(audioe2e.elf stream) failed (non-fatal)\n");
            }
        }
    }

    // 4.4 拉起阶段4 FP 演示进程 fpcheck（user-mode FP demo，会跑在 AP 上）。
    //    独立后台进程：真实 double 运算 + libc snprintf %.2f 输出，证明 AP 能跑
    //    用户态浮点/SSE 而无 #NM。RR 分发下它与 volumed/shell 轮流落各核。非致命。
    match libsys::exec_path("/programs/fpcheck.elf", &[]) {
        Ok(pid) => {
            let mut buf = [0u8; 8];
            let _ = write(STDOUT, b"[init] fpcheck started (pid ");
            let _ = write(STDOUT, dec_u64(pid, &mut buf));
            let _ = write(STDOUT, b")\n");
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] exec_path(fpcheck.elf) failed (non-fatal)\n");
        }
    }

    // 4.4.1 T1-8：端到端同进程双线程示例（threaddemo）。放风暴前执行并专候收尸，
    //    使风暴的 waitpid_any 不会误收 threaddemo 的僵尸。
    threaddemo_launch();

    // T2-0：真实 freestanding C 程序（x86-64 clang/lld 交叉链 + crt0）端到端。
    chelldemo_launch();

    // T2-3：C pthread 生命周期（create/join/detach/self）端到端。
    pthreaddemo_launch();

    // T2-4：C pthread 互斥/condvar/信号量（用户原子 + SYNC park）端到端。
    pthread_syncdemo_launch();

    // T2-5：真实 pthread 递归/join 基准（第三方惯用法）端到端。
    launch_c_prog("/programs/pthread_bench.elf", "pthread_bench");

    // A2 音频管道 e2e 已移至 4.2.5（**必须在 intel-hda 认领消费者槽之前**）。
    // 此处不再调用：消费者槽位独占，迟跑必然 EBUSY。详见该处说明。

    // 4.5 per-pid 锁化 + 跨核终止既有 bug 验证：跨核 spawn + SIGKILL terminate 风暴。
    cross_core_sigkill_storm();

    // 5. init 进入 supervisor 循环：拉起 shell → 等其退出 → 重生。
    //    类 SysV 登录循环语义，PID 1 永不退出。
    //    也负责收尸被过继给 init 的孤儿进程，并区分日志。
    let _ = write(STDOUT, b"[init] entering supervisor loop\n");
    let mut shell_pid: u64;
    loop {
        match libsys::exec_path("/programs/shell.elf", &[]) {
            Ok(pid) => {
                shell_pid = pid;
                let mut buf = [0u8; 8];
                let _ = write(STDOUT, b"[init] shell started (pid ");
                let _ = write(STDOUT, dec_u64(pid, &mut buf));
                let _ = write(STDOUT, b")\n");
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] exec_path(shell.elf) failed, retrying...\n");
                // 启动失败时短眠再试（避免忙转），走 TASK_WAIT(0, 500ms)
                let _ = libsys::sleep(500_000_000);
                continue;
            }
        }
        // 等任意子进程退出（shell 或被过继给 init 的孤儿）。
        match waitpid_any() {
            Ok(wr) => {
                // 检查 shell 是否还活着：读 /processes/{shell_pid}/status。
                // 若文件可读 → shell 还在，退出的是孤儿；
                // 若 NotFound → shell 没了，需要重生。
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
                    let mut buf = [0u8; 8];
                    let _ = write(STDOUT, b"[init] reaped orphan (code ");
                    let _ = write(STDOUT, dec_u64(wr.code, &mut buf));
                    let _ = write(STDOUT, b"), continuing\n");
                } else {
                    // shell 已退出 → 需要重生。
                    let mut buf = [0u8; 8];
                    let _ = write(STDOUT, b"[init] shell exited (code ");
                    let _ = write(STDOUT, dec_u64(wr.code, &mut buf));
                    let _ = write(STDOUT, b"), respawning\n");
                }
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] waitpid_any() failed, retrying\n");
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
