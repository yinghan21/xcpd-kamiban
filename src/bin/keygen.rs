//! 卡密生成器 —— **这个工具只有你自己用，绝对不要分发给客户**。
//!
//! 它持有 Ed25519 **私钥**，能签发卡密。私钥泄露 = 授权系统报废。
//!
//! # 用法
//!
//! ```bash
//! # ① 首次：生成密钥对（只做一次）。会写出 keygen.key 并打印公钥。
//! cargo run --release --bin keygen -- init
//!
//! # ② 把打印出来的公钥填进 src/license.rs 的 PUBLIC_KEY，然后重新编译 DLL
//!
//! # ③ 给客户发卡（默认绑定"运行 keygen 的这台机器"的机器码）：
//! cargo run --release --bin keygen -- issue --days 30
//!
//! # ④ 更常见：按客户提供的机器码发卡（客户在 DLL 界面上能看到自己的机器码）
//! cargo run --release --bin keygen -- issue --days 30 --machine A1B2C3D4
//!
//! # 查看公钥（随时可以再查）
//! cargo run --release --bin keygen -- show
//! ```
//!
//! ★ `keygen.key` 是你的私钥，**务必**：
//!   1. 不要提交到 git（.gitignore 里已排除 `keygen.key`）
//!   2. 备份到一个离线的地方（U 盘/密码管理器）
//!   3. 丢了 = 无法再签发新卡密（旧卡密仍然有效，因为验证靠公钥）

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

// ==================== 与 license.rs 保持一致的协议常量 ====================
// ★ 这几个值必须和 src/license.rs 里完全一致，改一个就要两边同步改，
//   否则生成出来的卡密会验不过。

const PAYLOAD_LEN: usize = 8;
const BASE32: data_encoding::Encoding = data_encoding::BASE32_NOPAD;
const KEY_FILE: &str = "keygen.key";

// ==================== 机器码（和 DLL 侧算法完全一致） ====================

fn volume_serial() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    let root: Vec<u16> = "C:\\\0".encode_utf16().collect();
    let mut serial: u32 = 0;
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 { 0 } else { serial }
}

/// ★ 必须和 license.rs 的 machine_code() 逐字节一致：
///   同样的域分离前缀、同样的哈希、同样取前 4 字节。
///   任何一处不一致 → 生成出来的卡密永远验不过。
fn machine_code() -> [u8; 4] {
    let mut h = Sha256::new();
    h.update(b"xcpd-machine-v1");
    h.update(volume_serial().to_le_bytes());
    let d = h.finalize();
    [d[0], d[1], d[2], d[3]]
}

/// 把"显示形式的机器码"（8 位十六进制）解析回 4 字节。
///
/// ★★ 这里**必须按字节对逐个解析**，绝不能写成
///    `u32::from_str_radix(s, 16)?.to_le_bytes()` ——
///    那样会把字节序整个反过来：输入 `A1B2C3D4` 得到 `[D4,C3,B2,A1]`，
///    而 DLL 侧显示的 `A1B2C3D4` 指的是 `[A1,B2,C3,D4]`。
///    两边算出的机器码不一致 → 签出来的卡永远报"与本机不匹配"，
///    而且这个症状完全看不出是转换环节出的问题（曾真实踩过）。
fn parse_machine_id(s: &str) -> Option<[u8; 4]> {
    let t: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if t.len() != 8 {
        return None;
    }
    let mut out = [0u8; 4];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn mc_display(mc: &[u8; 4]) -> String {
    format!("{:02X}{:02X}{:02X}{:02X}", mc[0], mc[1], mc[2], mc[3])
}

// ==================== 日期格式化（避免为一个显示需求拖进 chrono） ====================

/// 天数 → (年, 月, 日)。Howard Hinnant 的经典算法，纯整数运算、无依赖。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn format_ts(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    let (y, m, d) = civil_from_days(days);
    // ★ 这里按 UTC 换算（不处理时区）。对"看个大概什么时候到期"够用了；
    //   要精确到本地时间就得拖进 chrono 或调 Win32 API，不值得。
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 秒数说成人话（`1 天 3 小时` / `23 小时 59 分` / `5 分`）。
///
/// ★ 必须和 lib 侧 `license.rs` 的 `format_duration()` 保持一致 ——
///   两处是独立实现（bin 链接不到 cdylib），改一边记得同步另一边。
///
/// ★ 为什么不能直接显示"整数天"：`86400/86400=1` 看着没问题，但客户
///   签发完**几秒后**查询时剩余 86395 秒 → `86395/86400=0`，
///   一张刚签发的 1 天卡会显示"还剩 0 天"，客户会以为卡是坏的。
fn human_dur(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d} 天 {h} 小时")
    } else if h > 0 {
        format!("{h} 小时 {m} 分")
    } else {
        format!("{m} 分")
    }
}

// ==================== 命令实现 ====================

fn usage() {
    println!(
        r#"
卡密生成器 —— 只有你自己用，别分发

用法:
  keygen init                                  生成密钥对（只做一次）
  keygen issue --days 30                       签发卡密（绑定本机）
  keygen issue --days 30 --machine A1B2C3D4    签发卡密（绑定指定机器）
  keygen verify 卡密                           校验一张卡密
  keygen show                                  显示公钥

说明:
  --days     有效期天数，例如 30
  --machine  客户机器码（8 位十六进制）。不填则绑定**本机**机器码。
             客户在 DLL 的卡密窗口里能看到自己的机器码。

示例:
  keygen issue --days 7
  keygen issue --days 30 --machine 877D9464
  keygen verify "CZVMK2WU-YOZKC45E-V627NQYV-..."

★ PowerShell 用户注意：命令里不要写尖括号。
  网上很多文档用 <卡密> 表示"这里填卡密"，但 PowerShell 把 < 当重定向符号，
  照抄会直接报：The '<' operator is reserved for future use
  正确做法是**直接粘卡密**，或用引号包起来（推荐）：
    keygen verify "CZVMK2WU-..."
"#
    );
}

/// 生成密钥对：私钥写文件，公钥打印出来（要填进 license.rs）
fn cmd_init() {
    if std::path::Path::new(KEY_FILE).exists() {
        println!("⚠ {KEY_FILE} 已存在，拒绝覆盖（避免把正在用的私钥弄丢）。");
        println!("  真要重新生成，请先手动把它挪走。");
        return;
    }

    // ★ 用操作系统的密码学安全熵源。绝不能用 rand 的默认种子或时间戳 ——
    //   私钥的可预测性 = 授权系统的可破解性。
    let mut seed = [0u8; 32];
    if getrandom::getrandom(&mut seed).is_err() {
        println!("✗ 读取系统熵源失败");
        return;
    }

    let sk = SigningKey::from_bytes(&seed);
    let vk = sk.verifying_key();
    let pub_bytes = vk.to_bytes();

    if std::fs::write(KEY_FILE, seed).is_err() {
        println!("✗ 无法写入 {KEY_FILE}");
        return;
    }

    println!("\n================ 密钥对已生成 ================");
    println!("私钥文件：{KEY_FILE}  ★ 务必保管好，丢了就签不了新卡了，也不要提交到 git");
    println!("\n公钥（复制下面这一整段，填进 src/license.rs 的 PUBLIC_KEY）：\n");
    println!("pub const PUBLIC_KEY: [u8; 32] = [");
    for chunk in pub_bytes.chunks(8) {
        let line: Vec<String> = chunk.iter().map(|b| format!("0x{b:02X}")).collect();
        println!("    {},", line.join(", "));
    }
    println!("];");
    println!("\n填完记得重新编译 DLL：cargo build --release");
    println!("==============================================\n");
}

/// 显示当前公钥（从私钥推导）
fn cmd_show() {
    let Ok(seed) = std::fs::read(KEY_FILE) else {
        println!("✗ 读不到 {KEY_FILE}，先跑一次 `keygen init`");
        return;
    };
    let Ok(seed32) = <[u8; 32]>::try_from(seed.as_slice()) else {
        println!("✗ {KEY_FILE} 长度不对（应为 32 字节）");
        return;
    };
    let vk = SigningKey::from_bytes(&seed32).verifying_key();
    println!("\n公钥（填进 src/license.rs 的 PUBLIC_KEY）：\n");
    println!("pub const PUBLIC_KEY: [u8; 32] = [");
    let b = vk.to_bytes();
    for chunk in b.chunks(8) {
        let line: Vec<String> = chunk.iter().map(|x| format!("0x{x:02X}")).collect();
        println!("    {},", line.join(", "));
    }
    println!("];\n");
}

/// 校验一张卡密：确认签名是你签的、看绑定机器码和到期时间。
///
/// ★ 为什么 keygen 也要能验证：客户说"卡密用不了"的时候，
///   你先在这里验一遍，就能立刻分清是"卡本身有问题"还是"他环境的问题"，
///   省掉一轮来回扯皮。
fn cmd_verify(args: &[String]) {
    let Some(code) = args.first() else {
        println!("用法: keygen verify <卡密>");
        return;
    };

    // 自己派生公钥（和填进 license.rs 的那个是同一个，顺便能核对）
    let Ok(seed) = std::fs::read(KEY_FILE) else {
        println!("✗ 读不到 {KEY_FILE}，先跑一次 `keygen init`");
        return;
    };
    let Ok(seed32) = <[u8; 32]>::try_from(seed.as_slice()) else {
        println!("✗ {KEY_FILE} 长度不对");
        return;
    };
    let vk: VerifyingKey = SigningKey::from_bytes(&seed32).verifying_key();

    // 规整输入：和 DLL 侧完全一样的处理（去掉 `-`/空格、统一大写）
    let clean: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();

    let bytes = match BASE32.decode(clean.as_bytes()) {
        Ok(b) => b,
        Err(_) => {
            println!("✗ 不是合法的 Base32 卡密（含非法字符）");
            return;
        }
    };
    if bytes.len() != PAYLOAD_LEN + 64 {
        println!("✗ 长度不对：解出 {} 字节，应为 {}", bytes.len(), PAYLOAD_LEN + 64);
        return;
    }

    let payload = &bytes[..PAYLOAD_LEN];
    let sig_arr: [u8; 64] = bytes[PAYLOAD_LEN..].try_into().unwrap();
    let sig = Signature::from_bytes(&sig_arr);

    // ★ 验签：这一步能证明"这张卡确实是你用私钥签的"，而不是别人拼出来的
    if vk.verify(payload, &sig).is_err() {
        println!("✗ 签名无效 —— 不是本私钥签发的卡密（伪造，或者内容被改过）");
        return;
    }

    let expires = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as u64;
    let mc: [u8; 4] = payload[4..8].try_into().unwrap();
    let now = now_unix();

    println!("\n================ 卡密校验通过 ================");
    println!("签名：有效（确实由你签发）");
    println!("绑定机器码：{}", mc_display(&mc));
    println!("到期时间：{}", format_ts(expires));
    if expires > now {
        println!("状态：有效，还剩 {}", human_dur(expires - now));
    } else {
        println!("状态：**已过期** {}", human_dur(now - expires));
    }
    println!("==============================================\n");
}

/// 签发卡密
fn cmd_issue(args: &[String]) {    // ---- 解析参数 ----
    let mut days: Option<u64> = None;
    let mut machine: Option<[u8; 4]> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                days = args.get(i).and_then(|s| s.parse::<u64>().ok());
            }
            "--machine" => {
                i += 1;
                match args.get(i).and_then(|s| parse_machine_id(s)) {
                    Some(m) => machine = Some(m),
                    None => {
                        println!("✗ --machine 格式不对，应该是 8 位十六进制，例如 A1B2C3D4");
                        return;
                    }
                }
            }
            other => {
                println!("✗ 不认识的参数：{other}");
                usage();
                return;
            }
        }
        i += 1;
    }

    let Some(days) = days else {
        println!("✗ 必须指定 --days <天数>");
        usage();
        return;
    };
    if days == 0 {
        println!("✗ 天数必须大于 0");
        return;
    }

    // ---- 读私钥 ----
    let Ok(seed) = std::fs::read(KEY_FILE) else {
        println!("✗ 读不到 {KEY_FILE}，先跑一次 `keygen init`");
        return;
    };
    let Ok(seed32) = <[u8; 32]>::try_from(seed.as_slice()) else {
        println!("✗ {KEY_FILE} 长度不对（应为 32 字节）");
        return;
    };
    let sk = SigningKey::from_bytes(&seed32);

    // ---- 组装明文 ----
    // 不指定机器码时，绑定"运行 keygen 的这台机器"（适合自用/测试）
    let mc = machine.unwrap_or_else(machine_code);
    let expires = now_unix() + days * 86_400;

    let mut payload = [0u8; PAYLOAD_LEN];
    // ★ 到期时间存 u32：到 2106 年都够。用 u32 是为了把卡密压短 ——
    //   每一字节在 Base32 里都会变成 1.6 个字符，客户是要手抄的。
    payload[0..4].copy_from_slice(&(expires as u32).to_le_bytes());
    payload[4..8].copy_from_slice(&mc);

    // ---- 签名 ----
    // ★ 签名覆盖整个 payload，所以到期时间和机器码都改不了。
    let sig = sk.sign(&payload);

    // ---- 组装卡密 ----
    let mut all = Vec::with_capacity(PAYLOAD_LEN + 64);
    all.extend_from_slice(&payload);
    all.extend_from_slice(&sig.to_bytes());
    let code = BASE32.encode(&all);
    let pretty = pretty(&code);

    // ---- 打印 ----
    println!("\n================ 卡密签发成功 ================");
    println!("绑定机器码：{}", mc_display(&mc));
    println!(
        "有效期：{days} 天（到期 {}，{}）",
        format_ts(expires),
        human_dur(expires.saturating_sub(now_unix()))
    );
    println!("\n卡密（整段发给客户，让他粘贴进卡密窗口）：\n");
    println!("{pretty}");
    println!("\n字符数：{}（Base32，不含 0/1/8/9，不会有 O/0 混淆问题）", pretty.len());
    println!("==============================================\n");
}

/// 每 8 字符插一个 `-`（和 license.rs 的 prettify 行为一致）
fn pretty(code: &str) -> String {
    code.as_bytes()
        .chunks(8)
        .filter_map(|c| std::str::from_utf8(c).ok())
        .collect::<Vec<_>>()
        .join("-")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("init") => cmd_init(),
        Some("issue") => cmd_issue(&args[1..]),
        Some("verify") => cmd_verify(&args[1..]),
        Some("show") => cmd_show(),
        _ => usage(),
    }
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    /// 机器码的"显示 ↔ 解析"必须严格对应。
    ///
    /// ★ 这个测试是为一个**真实踩过的 bug** 补的：
    ///   早期 `parse_machine_id` 写成 `u32::from_str_radix(s,16)?.to_le_bytes()`，
    ///   把字节序整个反了 —— 输入 `A1B2C3D4` 得到 `[D4,C3,B2,A1]`。
    ///   后果：用 `--machine` 签出来的卡**永远验不过**，而且报错信息是
    ///   "卡密与本机不匹配"，完全看不出问题出在机器码转换上。
    #[test]
    fn machine_id_roundtrip() {
        // ① 本机机器码往返必须一致
        let mc = machine_code();
        let shown = mc_display(&mc);
        let parsed = parse_machine_id(&shown).expect("应当能解析回机器码");
        assert_eq!(parsed, mc, "显示/解析往返不一致 → 字节序反了");

        // ② 用固定字面量分别钉死两端的正确行为。
        //    ★ 这一步不能省：假如 mc_display 和 parse_machine_id
        //      **同时以相反的方式出错**，① 的往返测试会恰好通过，bug 被掩盖。
        //      只有拿已知值各自校准，才能保证它们真的对。
        assert_eq!(
            parse_machine_id("A1B2C3D4").unwrap(),
            [0xA1, 0xB2, 0xC3, 0xD4],
            "解析端字节序错误"
        );
        assert_eq!(
            mc_display(&[0xA1, 0xB2, 0xC3, 0xD4]),
            "A1B2C3D4",
            "显示端字节序错误"
        );

        // ③ 非法输入要能识别出来
        assert!(parse_machine_id("XYZ").is_none());
        assert!(parse_machine_id("A1B2C3").is_none(), "少一位应当拒绝");
        assert!(parse_machine_id("").is_none());
    }

    /// 剩余时长的显示必须精确到小时/分钟。
    ///
    /// ★ 这个测试钉的是一个**已经发生过的误导**：早期显示用的是整数天
    ///   `(expires - now) / 86400`，一张刚签发的"1 天卡"立刻查询会得到
    ///   `86395 / 86400 == 0`，界面上写着"还剩 0 天" —— 卡是好的，
    ///   但客户看到这个数字会直接怀疑卡有问题。
    #[test]
    fn human_dur_is_precise() {
        assert_eq!(human_dur(86_395), "23 小时 59 分", "不足一天必须显示小时，不能塌成 0 天");
        assert_eq!(human_dur(86_400), "1 天 0 小时");
        assert_eq!(human_dur(90_000), "1 天 1 小时");
        assert_eq!(human_dur(3_600), "1 小时 0 分");
        assert_eq!(human_dur(300), "5 分");
        assert_eq!(human_dur(0), "0 分");
    }
}
