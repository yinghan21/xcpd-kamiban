//! 极简限流日志。
//!
//! 内存读取线程每 2ms 跑一轮，直接写文件会刷爆磁盘，所以这里做了两层过滤：
//! 1. 状态没变化（和上一条一样）不写；
//! 2. 全局最小间隔 MIN_INTERVAL_MS，防止状态来回跳变时刷屏。
//!
//! 日志路径固定 C:\csgo_hook.log（游戏一般以管理员身份运行，有写根目录权限）。
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const LOG_PATH: &str = "C:\\csgo_hook.log";

/// 两条日志之间的最小间隔(毫秒)。状态持续异常时别刷屏。
const MIN_INTERVAL_MS: u64 = 1500;

static LAST_STAGE: Mutex<Option<&'static str>> = Mutex::new(None);
static LAST_WRITE_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn guard() -> std::sync::MutexGuard<'static, Option<&'static str>> {
    match LAST_STAGE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 清空日志并写一行会话头（每次注入时调用一次）。
pub fn reset(tag: &str) {
    *guard() = None;
    LAST_WRITE_MS.store(0, Ordering::Relaxed);
    if let Ok(mut f) = std::fs::File::create(LOG_PATH) {
        // 带 UTF-8 BOM，保证 Windows 记事本按 UTF-8 打开，中文不乱码。
        let _ = f.write_all(&[0xEF, 0xBB, 0xBF]);
        let _ = writeln!(f, "=== {tag} ===");
    }
}

/// 无条件追加一行，用于一次性错误信息。
pub fn append(line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// 状态变化时追加一行；与上一条相同、或距上一条太近，则忽略。
pub fn stage(stage: &'static str) {
    let now = now_ms();
    let prev = LAST_WRITE_MS.load(Ordering::Relaxed);
    if prev != 0 && now.saturating_sub(prev) < MIN_INTERVAL_MS {
        return;
    }

    let mut last = guard();
    if *last == Some(stage) {
        return;
    }
    *last = Some(stage);
    LAST_WRITE_MS.store(now, Ordering::Relaxed);
    drop(last);

    append(stage);
}
