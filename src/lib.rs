// 函数名沿用 UnityPlayer_module_base / GameAssembly_module_base 这种写法（早期就定下的），
// 不想为了 Rust 命名规范到处改名，这里统一放行。
#![allow(non_snake_case)]

mod offset;
mod config;
mod memory;
mod data;
mod esp;
mod aim;
mod log;
mod license;

use crate::config::{Config};
use crate::data::Snapshot;

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use anyhow;
use imgui::{Condition, Ui, *};
use hudhook::hooks::dx11::ImguiDx11Hooks;
use hudhook::{MessageFilter, RenderContext};
use hudhook::ImguiRenderLoop;
use hudhook::Hudhook;
use windows_sys::Win32::System::SystemServices::{DLL_PROCESS_ATTACH};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

/// 剪贴板格式：Unicode 文本。
///
/// ★ 这里自己定义而不是从 windows-sys import：该 crate 的
///   `Win32::System::DataExchange` 模块导出了 OpenClipboard 等**函数**，
///   却没有导出这个**常量**。它的值是 Windows SDK 里的固定定义
///   （`CF_UNICODETEXT = 13`），任何 Windows 版本都一样，自己写是安全的。
const CF_UNICODETEXT: u32 = 13;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_INSERT};
use windows_sys::Win32::UI::WindowsAndMessaging::{ClipCursor};

struct EspLoop {
    config: Config,
    snapshot: Arc<RwLock<Snapshot>>,
    prev_insert_down: bool,

    // ===================== 卡密授权 =====================
    /// 当前授权状态。未授权时渲染线程只画激活窗口，ESP / 自瞄全部不工作。
    license: license::License,
    /// 卡密输入框的缓冲
    lic_input: String,
    /// 给用户看的提示（错误原因）
    lic_msg: String,
    /// 是否已经尝试过读取本地卡密（只在第一帧读一次）
    lic_checked: bool,

    // ===================== 延迟启动数据线程 =====================
    /// 两个模块基址先存着 —— 授权通过后才用它启动读内存线程。
    unity_base: usize,
    gameassembly_base: usize,
    /// 读内存线程是否已启动（只启动一次）
    data_started: bool,
}

impl EspLoop {
    fn poll_menu_key(&mut self) {
        let down = unsafe { (GetAsyncKeyState(VK_INSERT as i32) as u16 & 0x8000) != 0 };
        if down && !self.prev_insert_down {
            self.config.menu_open = !self.config.menu_open;
        }
        self.prev_insert_down = down;
    }
}

fn draw_menu(ui: &Ui, config: &mut Config, lic: &mut license::License) {
    if !config.menu_open {
        return;
    }
    ui.window("CSGO ESP --dong")
        .size([460.0, 420.0], Condition::FirstUseEver)
        .position([20.0, 20.0], Condition::FirstUseEver)
        .build(|| {
            // ===== 自瞄 =====
            ui.checkbox("Enable auto-aim", &mut config.aim_enabled);
            if config.aim_enabled {
                ui.slider("FOV", 20.0, 600.0, &mut config.aim_fov);
                ui.slider("Smooth", 1.0, 10.0, &mut config.aim_smooth);
                ui.slider("speed", 0.1, 3.0, &mut config.aim_speed);
                ui.slider("瞄点上下移(米, 负=往下)", -1.5, 1.5, &mut config.aim_offset_y);
                ui.slider("粘滞阈值(像素, 0=永远瞄准心最近的)", 0.0, 60.0, &mut config.aim_stick_px);
                ui.slider("超前补偿(帧, 治自瞄落在移动敌人身后; 0=关)", 0.0, 8.0, &mut config.aim_screen_lead);
                ui.slider("预判提前量(秒)", 0.0, 0.6, &mut config.aim_lead_time);
                let mut head = config.aim_target == crate::config::SnapTarget::Head;
                if ui.checkbox("瞄头(不勾=瞄脚)", &mut head) {
                    config.aim_target = if head {
                        crate::config::SnapTarget::Head
                    } else {
                        crate::config::SnapTarget::Feet
                    };
                }
                ui.checkbox("只瞄敌人(不瞄队友)", &mut config.aim_enemy_only);
                ui.same_line();
                ui.checkbox("瞄人机(AI/bot)  ←人机模式必开", &mut config.aim_bots);
                ui.checkbox("按键触发自瞄(不勾=一直瞄)", &mut config.aim_hold_mode);
                ui.same_line();
                if ui.button(format!("按键: {}  ←点这里改", crate::aim::vk_name(config.aim_key))) {
                    crate::aim::begin_bind_key();
                }
                if crate::aim::binding() {
                    ui.same_line();
                    ui.text("等待按键… (Esc 取消)");
                }
            }

            ui.separator();
            // ===== 透视 =====
            ui.checkbox("Enable ESP", &mut config.esp_enabled);
            ui.same_line();
            ui.checkbox("Enable BOX", &mut config.draw_box_enabled);
            ui.same_line();
            ui.checkbox("Enable LINE", &mut config.draw_snapline);
            ui.same_line();
            ui.checkbox("Enable HEALTH", &mut config.draw_health_bar);
            ui.same_line();
            ui.checkbox("绘制昵称", &mut config.draw_name);

            ui.separator();
            // ===== 显示 =====
            ui.checkbox("敌我区分(敌人红/队友青)", &mut config.team_check);
            ui.same_line();
            ui.checkbox("顶部显示 绘制/背敌 人数", &mut config.draw_counter);
            ui.same_line();
            ui.checkbox("骨架(棍人)", &mut config.draw_skeleton);
            ui.checkbox("给自己画框(洋红SELF)", &mut config.draw_self_box);
            ui.same_line();
            ui.checkbox("隐藏离线/退出玩家(治“幽灵框”)", &mut config.hide_offline);
            ui.slider("框宽(米)", 0.3, 2.0, &mut config.box_width);
            ui.slider("框内边距(像素)", 0.0, 10.0, &mut config.box_pad);

            ui.separator();
            // ===== 头部 / 姿态（默认值就能用）=====
            ui.slider("身高(读不到真实头时的回退值)", -30.0, 30.0, &mut config.player_height);
            ui.slider("头顶微调(米, 盖住头顶)", -1.0, 1.0, &mut config.head_pad);
            ui.checkbox("用真头(客户端链 headPoint)", &mut config.use_real_head);
            ui.same_line();
            ui.checkbox("蹲下/趴下时框跟着变矮", &mut config.crouch_follow);
            ui.checkbox("位置平滑(治“框跟随一卡一卡”)", &mut config.pos_smooth);
            ui.same_line();
            ui.checkbox("探头跟随(侧身时框/瞄点跟着探出头)", &mut config.lean_follow);
            if config.lean_follow {
                ui.slider("  探头横向偏移(米)", 0.0, 1.0, &mut config.lean_shift_m);
                ui.same_line();
                ui.checkbox("  方向反转", &mut config.lean_invert);
            }

            ui.separator();
            // ===== 授权信息 / 换卡 =====
            // 卡密激活后会存到本地，之后每次启动自动读取、不再询问。
            // 续期（旧卡过期换新卡）或换机器时，必须先在这里把旧记录清掉，
            // 否则程序会一直读那张过期卡，连激活窗口都不会弹。
            if let license::License::Valid { expires } = &*lic {
                let days = license::remaining_days(*expires);
                ui.text_colored(
                    [0.4, 1.0, 0.5, 1.0],
                    &format!("授权有效 · 剩余 {days} 天"),
                );
            }
            if ui.button("注销授权（换卡 / 续期用）") {
                license::clear();
                *lic = license::License::Missing;
            }
            ui.same_line();
            ui.text("注销后激活窗口会重新出现，重新粘贴新卡即可");
        });
}

/// 卡密激活窗口。
///
/// ★ 这是"未授权"状态下唯一会画出来的东西 —— ESP / 自瞄的代码在这之前就 return 了。
///   客户第一次注入时会看到它：把机器码发给你换卡密，粘贴进来点验证即可。
///   验证通过后卡密会存到本地，之后每次启动自动读取，不用重复输入。
fn draw_license_window(ui: &Ui, st: &mut EspLoop) {
    let [sw, sh] = ui.io().display_size;

    // ★ 用 Condition::Always 而不是 FirstUseEver：这个窗口必须一直可见。
    //   用 FirstUseEver 的话它会"记住位置"，客户拖到屏幕外就找不回来了。
    ui.window("授权验证  ·  xcpd")
        .size([620.0, 340.0], Condition::Always)
        .position([sw * 0.5 - 310.0, sh * 0.5 - 170.0], Condition::Always)
        .build(|| {
            ui.text("本程序需要卡密授权");
            ui.separator();

            // ---- 机器码：客户要把这串发给你 ----
            ui.text("本机机器码：");
            ui.same_line();
            ui.text_colored([0.4, 1.0, 0.5, 1.0], &license::machine_id_display());
            ui.text("把上面这串发给我买卡 / 续期（卡密与它绑定，换机器要重新签）");

            ui.separator();

            // ---- 卡密输入 ----
            ui.text("卡密：");
            ui.input_text("##lic", &mut st.lic_input).build();
            ui.text("粘贴：Ctrl+V；输入框里 Ctrl+A 全选 / Ctrl+C 复制也可用");
            ui.text("（格式形如 XXXXXXXX-XXXXXXXX-...）");

            if ui.button("  验证并保存  ") {
                match license::verify(&st.lic_input) {
                    Ok(expires) => {
                        license::save(&st.lic_input);
                        st.license = license::License::Valid { expires };
                        st.lic_msg = String::new();
                    }
                    Err(e) => st.lic_msg = e,
                }
            }

            // ★ 保底通道：Windows 剪贴板是独占资源，万一被输入法/剪贴板工具占着，
            //   粘贴会失败。这时让客户把卡密存成文本文件，一键读取 —— 不用重启游戏。
            ui.same_line();
            if ui.button("  从文件读取  ") {
                match license::load() {
                    license::License::Valid { expires } => {
                        st.license = license::License::Valid { expires };
                        st.lic_msg = String::new();
                    }
                    license::License::Invalid(e) => st.lic_msg = e,
                    license::License::Missing => {
                        st.lic_msg = "读不到 C:\\csgo_hook.lic（文件不存在或内容为空）".to_string()
                    }
                }
            }

            // ---- 提示（失败原因直接给客户看，他一看就知道该找你换什么）----
            if !st.lic_msg.is_empty() {
                ui.separator();
                ui.text_colored([1.0, 0.4, 0.4, 1.0], &st.lic_msg);
            }
            // 从本地文件读出来的卡密如果无效（比如过期了），也把原因摆出来
            if let license::License::Invalid(e) = &st.license {
                ui.separator();
                ui.text_colored([1.0, 0.4, 0.4, 1.0], e);
            }
        });
}

impl ImguiRenderLoop for EspLoop {
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _rc: &'a mut dyn RenderContext) {
        // ★ 给 ImGui 接上真正的 Windows 剪贴板。
        //   少了这一行，输入框里的 Ctrl+V / Ctrl+C / Ctrl+A 全都无效 ——
        //   因为 imgui-rs 默认挂的是空实现（见 WinClipboard 的注释）。
        ctx.set_clipboard_backend(WinClipboard);

        // 必须在这里加字体：hudhook 紧接着才 build 字体图集，之后再加就晚了。
        load_chinese_font(ctx);
    }

    fn before_render<'a>(&'a mut self, ctx: &mut Context, _rc: &'a mut dyn RenderContext) {
        let io = ctx.io_mut();

        // ★ 未授权时也必须显示鼠标光标并解除锁定 ——
        //   否则客户没法在激活窗口里点「验证」按钮（游戏默认把光标锁在屏幕中心）。
        let needs_cursor = self.config.menu_open
            || !matches!(self.license, license::License::Valid { .. });
        io.mouse_draw_cursor = needs_cursor;

        if let Some((w,h)) = memory::game_client_size() {
            io.display_size = [w,h];
        }

        if needs_cursor {
            let _ = unsafe{ ClipCursor(std::ptr::null()) };
        }
    }

    fn render(&mut self, ui: &mut Ui) {
        // 标记“这一帧游戏能正常渲染” → 允许 IL2CPP API 调用。
        // 加载/切场景时 Present 不会跑，时间戳变旧，读线程就会自动停手，不再闪退。
        memory::api_tick();

        // ===================== 授权闸门 =====================
        // 第一帧：尝试读取本地保存的卡密（客户激活过一次之后就不用再输了）
        if !self.lic_checked {
            self.lic_checked = true;
            self.license = license::load();
            match &self.license {
                license::License::Valid { expires } => log::append(&format!(
                    "[授权] 卡密有效，剩余 {} 天 | 机器码 {}",
                    license::remaining_days(*expires),
                    license::machine_id_display()
                )),
                license::License::Invalid(e) => {
                    log::append(&format!("[授权] 本地卡密无效：{e}"))
                }
                license::License::Missing => log::append(&format!(
                    "[授权] 未找到卡密，等待输入 | 本机机器码 {}",
                    license::machine_id_display()
                )),
            }
        }

        // ★★ 未授权 → 只画激活窗口，然后直接返回。
        //    这一行是整个授权系统的执行点：ESP、自瞄、菜单全都在它后面，
        //    没通过就一行都执行不到。
        if !matches!(self.license, license::License::Valid { .. }) {
            draw_license_window(ui, self);
            return;
        }

        // 授权通过 → 启动读内存线程（只启动一次）。
        // ★ 放在这里而不是 run() 里：未授权的 DLL **连一个字节的游戏内存都不读**，
        //   它只是个会画输入框的空壳，被逆向也拿不到任何游戏数据。
        if !self.data_started {
            self.data_started = true;
            data::spawn(self.unity_base, self.gameassembly_base, Arc::clone(&self.snapshot));
        }

        // 正在等用户按一个键 → 捕获它作为自瞄按键
        if let Some(vk) = aim::poll_bind_key() {
            self.config.aim_key = vk;
        }
        self.poll_menu_key();

        let snapshot = match self.snapshot.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        data::LAYOUT.store(self.config.layout, Ordering::Relaxed);
        data::LIVE_Y_ADJUST.store(self.config.live_y_adjust.to_bits(), Ordering::Relaxed);
        data::USE_REAL_HEAD.store(self.config.use_real_head, Ordering::Relaxed);
        data::HIDE_OFFLINE.store(self.config.hide_offline, Ordering::Relaxed);
        data::POS_SMOOTH_ON.store(self.config.pos_smooth, Ordering::Relaxed);
        data::POS_SMOOTH_TAU_MS.store(self.config.pos_smooth_ms.max(0.0) as u64, Ordering::Relaxed);
        data::GHOST_FILTER.store(self.config.ghost_filter, Ordering::Relaxed);
        data::GHOST_FREEZE_MS.store(
            (self.config.ghost_freeze_s.max(0.1) * 1000.0) as u64,
            Ordering::Relaxed,
        );
        data::LEAN_ON.store(self.config.lean_smooth_on, Ordering::Relaxed);
        data::USE_ROLE_SCAN.store(self.config.use_role_scan, Ordering::Relaxed);

        esp::draw(ui, &self.config, &snapshot);
        draw_menu(ui, &mut self.config, &mut self.license);
    }

    fn message_filter(&self, io: &Io) -> MessageFilter {
        // ★ 未授权时也必须捕获鼠标/键盘 —— 激活窗口上的输入框和按钮需要它们，
        //   否则客户连卡密都粘不进去。
        let need_input = self.config.menu_open
            || !matches!(self.license, license::License::Valid { .. });
        if !need_input {
            return MessageFilter::empty();
        }
        let mut filter = MessageFilter::empty();

        if io.want_capture_mouse {
            filter |= MessageFilter::InputMouse | MessageFilter::InputRaw;
        }
        if io.want_capture_keyboard {
            filter |= MessageFilter::InputKeyboard;
        }
        filter
    }
}

/// 给 ImGui 接上**真正的** Windows 剪贴板。
///
/// # 为什么必须自己写这个
/// imgui-rs 默认挂的剪贴板后端是 `DummyClipboardContext` —— 源码里明写着
/// "Non-functioning placeholder"：`get()` 恒返回 None，`set()` 什么都不做。
/// 而 hudhook 也从未替我们注册过。**两边都没做**，结果就是
/// ImGui 的输入框里 Ctrl+V 毫无反应 —— 客户根本没法把卡密粘贴进去。
///
/// # 为什么不引第三方库
/// `arboard` 之类会拖进一堆依赖、内部还会起线程。这里只需要读写
/// `CF_UNICODETEXT` 一种格式，直接调 Win32 更干净也更快。
struct WinClipboard;

/// 打开剪贴板，失败就短暂重试。
///
/// ★ 为什么要重试：Windows 剪贴板是**全局独占资源**，同一时刻只能有一个进程打开它。
///   如果别的程序（浏览器、输入法、剪贴板历史工具）正占着，`OpenClipboard` 会直接失败。
///   不重试的话，用户会偶发地遇到"点了粘贴没反应"，而且完全无从解释。
fn open_clipboard_retry() -> bool {
    for _ in 0..5 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

impl imgui::ClipboardBackend for WinClipboard {
    /// 读剪贴板（用户在输入框按 Ctrl+V 时 ImGui 会调它）
    fn get(&mut self) -> Option<String> {
        unsafe {
            if !open_clipboard_retry() {
                return None;
            }
            // ★ 用闭包包住读取逻辑，保证无论从哪条路径 return 都会执行 CloseClipboard。
            //   剪贴板不关的后果很严重：**整个系统的复制粘贴都会卡死**。
            let out = (|| {
                let h = GetClipboardData(CF_UNICODETEXT);
                if h.is_null() {
                    return None; // 剪贴板里不是文本（比如是图片）
                }
                let p = GlobalLock(h) as *const u16;
                if p.is_null() {
                    return None;
                }
                // 剪贴板文本是 UTF-16 且以 NUL 结尾，先量长度再切切片
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
                GlobalUnlock(h);
                Some(s)
            })();
            CloseClipboard();
            out
        }
    }

    /// 写剪贴板（ImGui 里选中文本按 Ctrl+C 时用）
    fn set(&mut self, value: &str) {
        unsafe {
            if !open_clipboard_retry() {
                return;
            }
            EmptyClipboard();

            // UTF-16 + 结尾 NUL
            let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = wide.len() * std::mem::size_of::<u16>();

            // ★ 必须用 GMEM_MOVEABLE：SetClipboardData 要求可移动的全局内存块，
            //   传 GMEM_FIXED 分配的内存会被系统直接拒绝。
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !h.is_null() {
                let p = GlobalLock(h) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    GlobalUnlock(h);
                    // ★ SetClipboardData 成功后，这块内存的**所有权就移交给系统了**：
                    //   不能再碰它、更不能 GlobalFree —— 多释放一次会直接把剪贴板搞坏。
                    SetClipboardData(CF_UNICODETEXT, h);
                }
            }
            CloseClipboard();
        }
    }
}

/// ImGui 默认字体 (ProggyClean) 不含中文字形，中文会显示成乱码。
/// 这里从系统字体目录挑一个中文字体加进字体图集。
fn load_chinese_font(ctx: &mut Context) {    const FONT_SIZE: f32 = 18.0;

    const CANDIDATES: &[&str] = &[
        "C:\\Windows\\Fonts\\msyh.ttc",   // 微软雅黑
        "C:\\Windows\\Fonts\\msyh.ttf",
        "C:\\Windows\\Fonts\\msyhl.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",  // 黑体
        "C:\\Windows\\Fonts\\simsun.ttc",  // 宋体
        "C:\\Windows\\Fonts\\Deng.ttf",    // 等线
    ];

    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };

        // 字体数据必须活到进程结束，泄漏一份即可（只会加载一次）。
        let data: &'static [u8] = Box::leak(bytes.into_boxed_slice());

        ctx.fonts().add_font(&[FontSource::TtfData {
            data,
            size_pixels: FONT_SIZE,
            config: Some(FontConfig {
                size_pixels: FONT_SIZE,
                glyph_ranges: FontGlyphRanges::chinese_simplified_common(),
                oversample_h: 1,
                pixel_snap_h: true,
                ..Default::default()
            }),
        }]);

        crate::log::stage("正常: 已加载中文字体");
        return;
    }

    crate::log::stage("警告: 没找到系统中文字体, 菜单中文会显示为乱码");
}

/// 单实例占位：同一个进程里第二次注入会因为锁文件被独占打开而失败。
/// 返回的句柄必须一直活着（调用方持有），进程退出时自动失效。
fn claim_single_instance() -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .share_mode(0)
        .open("C:\\csgo_hook.lock")
        .ok()
}

fn run(hmodule: *mut c_void) ->anyhow::Result<()> {
    // ★ 防双重框：同一个游戏进程里注入了两份 DLL 的话，两份都会挂钩 Present → 每个敌人画两遍框。
    // 这里用锁文件挡住后注入的那一份（它不挂钩、直接返回）。
    let _instance_lock = match claim_single_instance() {
        Some(f) => f,
        None => {
            log::append("检测到已有另一个 xcpd 实例在运行 → 本实例不挂钩（双重框就是两份 DLL 各画一遍）");
            return Ok(());
        }
    };

    log::reset("xcpd ESP 启动 【v126·人机自瞄放行+人机诊断】");

    let unity_base = memory::UnityPlayer_module_base().ok_or_else(|| {
        log::stage("失败: GetModuleHandleA UnityPlayer.dll");
        anyhow::anyhow!("GetModuleHandleA UnityPlayer.dll 失败")
    })?;
    let gameassembly_base = memory::GameAssembly_module_base().ok_or_else(|| {
        log::stage("失败: GetModuleHandleA GameAssembly.dll");
        anyhow::anyhow!("GetModuleHandleA GameAssembly.dll 失败")
    })?;

    log::stage("正常: 两个模块基址都拿到了");
    let snapshot = Arc::new(RwLock::new(Snapshot::default()));

    // ★ 这里**故意不调** data::spawn —— 读内存线程改由渲染线程在
    //   "卡密验证通过"之后再启动。未授权的 DLL 因此完全不碰游戏数据。

    let render_loop = EspLoop {
        config: Config::default(),
        snapshot,
        prev_insert_down: false,
        license: license::License::Missing,
        lic_input: String::new(),
        lic_msg: String::new(),
        lic_checked: false,
        unity_base,
        gameassembly_base,
        data_started: false,
    };
    Hudhook::builder()
        .with::<ImguiDx11Hooks>(render_loop)
        .with_hmodule(hudhook::windows::Win32::Foundation::HINSTANCE(hmodule))
        .build()
        .apply()
        .map_err(|e| {
            log::stage("失败: hudhook 挂钩失败(不是 DX11 渲染?) " );
            anyhow::anyhow!("hudhook 挂钩失败: {e:?}")
        })?;

    log::stage("正常: hudhook 挂钩成功, 渲染线程已启动");
    Ok(())
}

#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "system" fn DllMain(module: *mut c_void, call_reason: u32, _: *mut c_void ) -> bool {
    if call_reason == DLL_PROCESS_ATTACH {
        let module_raw = module as usize;
        std::thread::spawn(move || {
            if let Err(err) = run(module_raw as *mut c_void) {
                log::append(&format!("初始化失败: {err:?}"));

            }
        });
    }
    true
}
