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
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_INSERT};
use windows_sys::Win32::UI::WindowsAndMessaging::{ClipCursor};

struct EspLoop {
    config: Config,
    snapshot: Arc<RwLock<Snapshot>>,
    prev_insert_down: bool,
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

fn draw_menu(ui: &Ui, config: &mut Config) {
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
        });
}

impl ImguiRenderLoop for EspLoop {
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _rc: &'a mut dyn RenderContext) {
        // 必须在这里加字体：hudhook 紧接着才 build 字体图集，之后再加就晚了。
        load_chinese_font(ctx);
    }

    fn before_render<'a>(&'a mut self, ctx: &mut Context, _rc: &'a mut dyn RenderContext) {
        let io = ctx.io_mut();
        io.mouse_draw_cursor = self.config.menu_open;
        if let Some((w,h)) = memory::game_client_size() {
            io.display_size = [w,h];
        }

        if self.config.menu_open {
            let _ = unsafe{ ClipCursor(std::ptr::null()) };
        }
    }

    fn render(&mut self, ui: &mut Ui) {
        // 标记“这一帧游戏能正常渲染” → 允许 IL2CPP API 调用。
        // 加载/切场景时 Present 不会跑，时间戳变旧，读线程就会自动停手，不再闪退。
        memory::api_tick();
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
        draw_menu(ui, &mut self.config);
    }

    fn message_filter(&self, io: &Io) -> MessageFilter {
        if !self.config.menu_open {
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

/// ImGui 默认字体 (ProggyClean) 不含中文字形，中文会显示成乱码。
/// 这里从系统字体目录挑一个中文字体加进字体图集。
fn load_chinese_font(ctx: &mut Context) {
    const FONT_SIZE: f32 = 18.0;

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
    data::spawn(unity_base, gameassembly_base, Arc::clone(&snapshot));

    let render_loop = EspLoop {
        config: Config::default(),
        snapshot,
        prev_insert_down: false,
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
