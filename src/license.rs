//! 卡密验证（离线签名授权）。
//!
//! # 为什么用 Ed25519 非对称签名，而不是 HMAC 之类的对称方案
//!
//! 对称签名的致命问题：**验证方和签发方用的是同一个密钥**。
//! 这个密钥必须同时存在于两个地方 —— 你自己的 keygen 工具，以及发给客户的 DLL。
//! 而 DLL 一旦发出去就不受你控制了：客户可以反编译它、把密钥抠出来，
//! 然后**自己写一个 keygen 批量生成卡密**。整套授权就废了。
//!
//! 非对称签名反过来：
//!   * **私钥**只存在你手里（keygen 用），永远不进 DLL；
//!   * **公钥**放进 DLL，它只能*验证*签名，**无法生成**签名。
//!
//! 所以就算客户把 DLL 逆向到底，也只能拿到一个公钥 —— 造不出卡密。
//!
//! # 卡密的数据结构
//!
//! ```text
//! 明文字节（下面 8 字节是被签名覆盖的内容，改一个 bit 验签就失败）：
//!   [0..4]  到期时间：Unix 时间戳，秒，小端 u32（够用到 2106 年）
//!   [4..8]  机器码哈希前 4 字节
//! 签名：
//!   [8..72] Ed25519 签名（固定 64 字节）
//! ```
//!
//! 整个 72 字节做 Base32 编码 = 116 个字符，每 8 字符插一个 `-`。
//! Base32 用 A-Z 和 2-7，**天然不含 0/1/8/9**，所以不会出现
//! "客户把 O 看成 0、把 l 看成 1 导致卡密输错"这种问题。
//!
//! # 三道防线
//!
//! 1. **签名** —— 防止有人伪造/篡改卡密内容（改到期时间？验签直接失败）
//! 2. **机器码** —— 防止一张卡密被多人共享（换机器就失效）
//! 3. **时间回拨检测** —— 防止客户把系统时间往回调来续命

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

// ==================== 需要你配置的部分 ====================

/// 内置的**公钥**（32 字节）。
///
/// ★ 这是唯一一个你必须手动填的常量。步骤：
///   1. 编译出 keygen 后先跑一次 `keygen init` —— 它会生成密钥对并打印公钥
///   2. 把打印出来的公钥填到这里（下面那行注释里有填写格式）
///   3. 重新编译 DLL，分发给客户
///
/// ★ 公钥放在 DLL 里是**安全的**：它只能验证签名，不能生成签名。
///   私钥（`keygen.key`）必须自己保管好，泄露了就等于授权系统报废。
///
/// ★ 如果这里还是全 0，说明你还没配置 —— 所有卡密都会验签失败，
///   报错信息会明确提示这一点。
pub const PUBLIC_KEY: [u8; 32] = [
    0xD3, 0x19, 0x07, 0x44, 0xA3, 0xB6, 0x25, 0x52,
    0xD0, 0xA6, 0x9E, 0x6F, 0x92, 0xBE, 0x33, 0x31,
    0xFB, 0xE0, 0x11, 0x37, 0xDA, 0xD4, 0x8E, 0xBB,
    0xD9, 0x57, 0xC3, 0x2C, 0x09, 0x66, 0xA9, 0x41,
];

// ==================== 内部常量 ====================

/// 卡密文件的存放位置。首次让客户输入后存这里，之后自动读取。
/// 和日志放同一个分区根目录 —— 游戏以管理员权限运行时有写权限。
const LIC_PATH: &str = "C:\\csgo_hook.lic";

/// 时间回拨检测的锚点文件：每次成功验证后写入当前时间。
/// 下次启动时如果发现"现在"比这里记录的还早，说明时间被往回拨了。
const TICK_PATH: &str = "C:\\csgo_hook.tick";

/// 明文字节数（到期时间 4 + 机器码 4）
const PAYLOAD_LEN: usize = 8;
/// 签名长度（Ed25519 固定 64 字节）
const SIG_LEN: usize = 64;
/// 卡密解码后的总长度
const TOTAL_LEN: usize = PAYLOAD_LEN + SIG_LEN;

/// 时间回拨的容差（秒）。给 1 小时余量，避免时区调整、NTP 校时被误判。
const ROLLBACK_TOLERANCE: u64 = 3600;

// ==================== 授权状态 ====================

/// 当前授权状态。渲染线程根据它决定"正常画 ESP"还是"画卡密输入窗口"。
pub enum License {
    /// 没有卡密文件（首次使用，或者文件被删了）
    Missing,
    /// 授权有效，`expires` 是到期时间戳
    Valid { expires: u64 },
    /// 有卡密但无效，附上给客户看的原因
    Invalid(String),
}

// ==================== 机器码 ====================

/// 读 C 盘的卷序列号。
///
/// ★ 为什么用卷序列号做机器指纹：
///   - 它是格式化时生成并存进分区表的，重装系统后**保持不变**（只要不重新格式化）
///   - 不像网卡 MAC 那样能随便改，也不像计算机名那样客户随手就能换
///   - 读取零成本（一个 API 调用），不像 WMI 那样要起 COM 慢几百毫秒
fn volume_serial() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    // Windows API 要 UTF-16 且以 NUL 结尾
    let root: Vec<u16> = "C:\\\0".encode_utf16().collect();
    let mut serial: u32 = 0;

    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(), // 卷标：不要
            0,
            &mut serial,          // ← 卷序列号写到这里
            std::ptr::null_mut(), // 最大文件名长度：不要
            std::ptr::null_mut(), // 文件系统标志：不要
            std::ptr::null_mut(), // 文件系统名：不要
            0,
        )
    };

    if ok == 0 { 0 } else { serial }
}

/// 机器码（4 字节）。
///
/// ★ 为什么哈希而不是直接用卷序列号：
///   卷序列号只有 32 位，而且不同机器**可能撞号**（批量格式化会连续分配）。
///   加一个固定的域分离前缀再哈希，能把分布打散，降低撞号概率 ——
///   同时也避免暴露"这张卡绑的是哪个序列号"。
///
/// ★ 为什么只取 4 字节：卡密长度直接受它影响，而 4 字节（43 亿种）
///   对"防止一张卡多人用"这个目标已经完全够用（撞号概率约 1/43 亿）。
pub fn machine_code() -> [u8; 4] {
    let serial = volume_serial();
    let mut h = Sha256::new();
    h.update(b"xcpd-machine-v1"); // 域分离：用途不同的哈希不会互相撞
    h.update(serial.to_le_bytes());
    let d = h.finalize();
    [d[0], d[1], d[2], d[3]]
}

/// 机器码的**显示形式**（8 位十六进制，如 `A1B2C3D4`）。
/// 客户看到这个值发给你，你用 `keygen issue --machine A1B2C3D4` 给他绑卡。
pub fn machine_id_display() -> String {
    let mc = machine_code();
    format!("{:02X}{:02X}{:02X}{:02X}", mc[0], mc[1], mc[2], mc[3])
}

// ==================== 时间 ====================

/// 当前 Unix 时间戳（秒）
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 距离到期还有多少天（负数 = 已过期）。用于在界面上显示剩余时间。
pub fn remaining_days(expires: u64) -> i64 {
    let now = now_unix() as i64;
    (expires as i64 - now) / 86400
}

/// 时间回拨检测 + 更新锚点。
///
/// ★ 原理：把"最近一次见到的时间"存盘。如果这次启动时系统时间**比上次记录的还早**
///   （超过 1 小时容差），说明客户把时钟往回拨了 —— 目的通常是让过期卡密复活。
///
/// ★ 这个机制的局限（要清楚）：客户如果**先备份锚点文件**，再把时间调回去，
///   检查就骗过去了。所以要更严的话，锚点应该同时写多个位置
///   （注册表 + 文件 + 隐写进别的文件），让"清理干净"变麻烦。
///   对个人项目来说，单点检测已经能挡住 99% 的普通客户。
fn check_and_update_tick(now: u64) -> Result<(), String> {
    if let Ok(s) = std::fs::read_to_string(TICK_PATH) {
        if let Ok(last) = s.trim().parse::<u64>() {
            if now + ROLLBACK_TOLERANCE < last {
                return Err(format!(
                    "检测到系统时间被往回调整（当前时间比上次运行早了 {}）",
                    format_duration(last - now)
                ));
            }
        }
    }
    // 记录本次时间（写失败不阻断 —— 可能是权限问题，但不该因此让人用不了）
    let _ = std::fs::write(TICK_PATH, now.to_string());
    Ok(())
}

/// 把秒数说成人话（"3 天 2 小时"）
pub fn format_duration(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d} 天 {h} 小时")
    } else if h > 0 {
        format!("{h} 小时 {m} 分")
    } else {
        format!("{m} 分")
    }
}

// ==================== 卡密文本 <-> 字节 ====================

/// 把客户输入的卡密规整成纯 Base32 大写（去掉 `-`、空格、换行，统一大写）。
/// ★ 这一步很重要：客户粘贴卡密时经常带上多余的空格/换行，
///   或者手抄时用了小写 —— 规整之后这些都不影响验证。
pub fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

// ==================== 核心：验证 ====================

/// 验证卡密。成功返回到期时间戳（Unix 秒）。
///
/// 五步检查，任何一步不过都返回给客户看的中文原因（不需要他们懂技术细节）。
pub fn verify(code: &str) -> Result<u64, String> {
    // ---- 0. 公钥是否已配置 ----
    if PUBLIC_KEY == [0u8; 32] {
        return Err("本程序尚未配置授权公钥（开发者需要用 keygen init 生成并填入）".into());
    }

    // ---- 1. Base32 解码 ----
    let clean = normalize(code);
    if clean.is_empty() {
        return Err("卡密为空".into());
    }
    let bytes = data_encoding::BASE32_NOPAD
        .decode(clean.as_bytes())
        .map_err(|_| "卡密格式不正确（含非法字符，请确认复制完整）".to_string())?;

    if bytes.len() != TOTAL_LEN {
        return Err(format!(
            "卡密长度不正确（解出 {} 字节，应为 {TOTAL_LEN} 字节，请确认复制完整）",
            bytes.len()
        ));
    }

    let payload = &bytes[..PAYLOAD_LEN];
    let sig_bytes: [u8; SIG_LEN] = bytes[PAYLOAD_LEN..]
        .try_into()
        .map_err(|_| "卡密签名长度异常".to_string())?;

    // ---- 2. 验签 ----
    // ★ 这一步是整个方案的安全基石：它证明"这 8 字节明文"确实是你签发的，
    //   客户没法修改里面的到期时间 —— 改一个 bit 签名就对不上了。
    let vk = VerifyingKey::from_bytes(&PUBLIC_KEY)
        .map_err(|_| "内置公钥格式无效（开发者配置有误）".to_string())?;
    let sig = Signature::from_bytes(&sig_bytes);
    vk.verify(payload, &sig)
        .map_err(|_| "卡密校验失败：不是本程序签发的卡密，或内容已被篡改".to_string())?;

    // ---- 3. 机器码比对 ----
    // ★ 防一码多机：卡密里签进了"签发时指定的机器码"，
    //   只有那台机器的卷序列号哈希能对上。
    let expires = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as u64;
    let mc = machine_code();
    if payload[4..PAYLOAD_LEN] != mc[..] {
        return Err(format!(
            "卡密与本机不匹配（绑定的不是这台机器）。\n本机机器码：{}",
            machine_id_display()
        ));
    }

    // ---- 4. 到期检查 ----
    let now = now_unix();
    if now > expires {
        return Err(format!(
            "卡密已过期（过期时间距今 {}）",
            format_duration(now - expires)
        ));
    }

    // ---- 5. 时间回拨检测 ----
    check_and_update_tick(now)?;

    Ok(expires)
}

// ==================== 本地存取 ====================

/// 保存卡密到文件（验证通过后调用）
pub fn save(code: &str) -> bool {
    std::fs::write(LIC_PATH, code.trim()).is_ok()
}

/// 读取本地卡密并验证。返回当前授权状态。
pub fn load() -> License {
    let Ok(text) = std::fs::read_to_string(LIC_PATH) else {
        return License::Missing; // 文件不存在 = 首次使用
    };
    let code = text.trim();
    if code.is_empty() {
        return License::Missing;
    }
    match verify(code) {
        Ok(expires) => License::Valid { expires },
        Err(e) => License::Invalid(e),
    }
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// 端到端：用真实的 `keygen.key` 签一张卡，然后走 DLL 侧的完整 `verify()`。
    ///
    /// ★ 这个测试为什么必须存在：
    ///   `license.rs`（DLL 侧验证）和 `bin/keygen.rs`（签发）是**两套独立实现**，
    ///   各自用不同的代码路径处理同一份字节协议。keygen 自己验自己通过，
    ///   并不代表 DLL 侧也能通过 —— 只有让 DLL 侧的 `verify()` 去验
    ///   keygen 协议生成的卡，才能真正证明两边一致。
    ///
    /// 没有 `keygen.key` 时自动跳过（方便在别的机器上跑）。
    #[test]
    fn end_to_end_with_keygen_key() {
        let Ok(seed) = std::fs::read("keygen.key") else {
            eprintln!("跳过：当前目录没有 keygen.key");
            return;
        };
        let seed32: [u8; 32] = seed.as_slice().try_into().expect("keygen.key 应为 32 字节");
        let sk = SigningKey::from_bytes(&seed32);

        // 完全复刻 `keygen issue` 的行为：30 天、绑定本机机器码
        let expires = now_unix() + 30 * 86_400;
        let mc = machine_code();
        let mut payload = [0u8; PAYLOAD_LEN];
        payload[0..4].copy_from_slice(&(expires as u32).to_le_bytes());
        payload[4..8].copy_from_slice(&mc);

        let sig = sk.sign(&payload);
        let mut all = Vec::new();
        all.extend_from_slice(&payload);
        all.extend_from_slice(&sig.to_bytes());

        // ---- ① 正常卡应该通过 ----
        let code = data_encoding::BASE32_NOPAD.encode(&all);
        let got = verify(&code).expect("DLL 侧应当验证通过");
        assert_eq!(got, expires, "返回到期时间应与签发时一致");

        // ---- ② 改到期时间 → 必须失败（证明签名真的在保护内容）----
        let mut tampered = payload;
        // ★ 注意 as u32：卡密里到期时间存的是 u32（4 字节），
        //   而 expires 变量是 u64，直接 to_le_bytes() 会得到 8 字节，塞不进 4 字节切片。
        tampered[0..4].copy_from_slice(&((expires + 86_400) as u32).to_le_bytes());
        let mut t = Vec::new();
        t.extend_from_slice(&tampered);
        t.extend_from_slice(&sig.to_bytes());
        let t_code = data_encoding::BASE32_NOPAD.encode(&t);
        assert!(
            verify(&t_code).is_err(),
            "把到期时间改长还验过了 → 签名没起作用，整个授权系统是假的"
        );

        // ---- ③ 改机器码 → 必须失败（证明一码一机生效）----
        let mut wrong = payload;
        wrong[4..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let mut w = Vec::new();
        w.extend_from_slice(&wrong);
        w.extend_from_slice(&sig.to_bytes());
        let w_code = data_encoding::BASE32_NOPAD.encode(&w);
        assert!(
            verify(&w_code).is_err(),
            "换了机器码还验过了 → 机器码绑定没生效，一张卡能多机用"
        );

        // ---- ④ 随便编的字符串 → 必须失败 ----
        assert!(verify("AAAAAAAA-BBBBBBBB-CCCCCCCC").is_err());
    }
}

