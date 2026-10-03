//! 用户态程序链接脚本注入。
//!
//! 通过 `CARGO_MANIFEST_DIR` 得到绝对路径，把 `linker.ld` 传给链接器，
//! 将 `.text` 等段定位到用户态地址（0x400000 起），`ENTRY(_start)`。
//!
//! `-no-pie`：强制生成 ET_EXEC（非 PIE）。rust-lld 默认生成 PIE（ET_DYN），
//! 而内核 ELF 加载器雏形只接受 ET_EXEC；裸机静态程序无需重定位，故关闭 PIE。

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    println!("cargo:rustc-link-arg=-no-pie");

    // A2-5：把"非交互跑 selftest quick"作为**构建期开关**注入 init。
    //
    // **为何是构建期而非运行期 argv**：内核在生产路径以**无 argv** 方式 spawn
    // init（`spawn_with_ppid_fds(0, "init.elf", ...)` 不带 argv 数组），且本仓无
    // kernel cmdline 机制。故 init 的 argv 在本环境恒为空——既有的 `--selftest`
    // 同理只能靠**重编译**切换。此处循同一手法，用环境变量在构建时决定是否
    // 预置该 argv，使验收可在自动化构建中跑到（否则 shell 的 stdin 是 PS/2 键盘，
    // 无法从串口日志驱动输入行）。
    println!("cargo:rerun-if-env-changed=BORUIX_INIT_ARGS");
    if let Ok(extra) = std::env::var("BORUIX_INIT_ARGS") {
        println!("cargo:rustc-env=BORUIX_INIT_ARGS={}", extra);
    }

    // BORUIX_INIT_RUN：同一注入手法的第二种形态——**不按空白切分**，整串原样作为
    // --run= 的命令行交给 shell。为什么需要它：BORUIX_INIT_ARGS 按空白切分，无法注入
    // "程序名 + 长参数"形态（实测 --run=echo hello 只传了 echo），而 3P4-2 的验收要求
    // "实测一条 >511B 的命令行"，cc1 那类超长参数也需要它。
    println!("cargo:rerun-if-env-changed=BORUIX_INIT_RUN");
    if let Ok(raw) = std::env::var("BORUIX_INIT_RUN") {
        println!("cargo:rustc-env=BORUIX_INIT_RUN={}", raw);
    }

    // ADR-048 扩展 E1（owner 指令 2026-09-27）：console 实例总数与内核 vfs 侧
    // 同源注入（vfs/build.rs 同款钳位 1..=256、默认 4）——init 的守护 spawn
    // 循环与会话轮转位跟 N 走，与 devfs 挂载的实例族形状**同源对齐**（S13：
    // 同一个数字只允许一个真相来源，两侧从同一个 env 读）。
    println!("cargo:rerun-if-env-changed=BORUIX_CONSOLES_N");
    let n: u32 = std::env::var("BORUIX_CONSOLES_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let n = n.clamp(1, 256);
    println!("cargo:rustc-env=BORUIX_CONSOLES_N={}", n);

    // ADR-048 扩展 E3（owner 指令「并行多会话」）：会话模式构建期开关——
    // serial（默认，=T5 既有轮转形态，零变化判据）| parallel（每实例一个
    // login/shell 同时在场，getty 重生即焦点转移）。为什么构建期：模式决定
    // supervisor 的结构（串行等待 vs 账本对账），不是可调参数。
    println!("cargo:rerun-if-env-changed=BORUIX_SESSION_MODE");
    let mode = std::env::var("BORUIX_SESSION_MODE").unwrap_or_default();
    let mode = match mode.as_str() {
        "parallel" => "parallel",
        _ => "serial",
    };
    println!("cargo:rustc-env=BORUIX_SESSION_MODE={}", mode);
}
