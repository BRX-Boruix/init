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
}
