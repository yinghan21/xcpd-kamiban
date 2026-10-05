//! 自瞄：算目标 → 屏幕误差 → 换算成鼠标位移 → SendInput。
//!
//! 跑在渲染帧里（Present 之后），所以这一帧发出的位移下一帧才生效。
//! 只在「开关打开 + 按住 aim_key（默认右键）」时工作；菜单开着时不工作。
//!
//! 防晃动（这版重点重写）：
//!   0. **目标屏幕坐标 EMA 平滑**：原始投影坐标每帧都在小幅跳（脚/头去抖、姿态缩放、矩阵浮点），
//!      直接拿它算误差准心就会抖 —— 先平滑再算；换目标立即重置，不拖影。
//!   1. **自适应换算**：不知道你游戏内灵敏度是多少，所以用「上一帧发了多少计数 / 误差减少了多少像素」
//!      反推「每像素需要多少计数」，边瞄边学（EMA 平滑）。这样不管灵敏度是 2 还是 30 都不会越冲。
//!   2. 死区：目标离准心 < DEAD_ZONE_PX 像素就完全不动鼠标；
//!   3. 限步 + 增益：单帧位移不超过 MAX_STEP，且只走剩余误差的一部分（gain < 1 → 只会收敛不会振荡）。
//!   4. 锁定目标：优先沿用上一帧锁定的那个目标，避免每帧换人导致准心左右横跳。
//! 另外：自己的框（ptr 相同）和贴在自己身上（< 2 米）的目标一律不瞄。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};

use crate::config::{Config, SnapTarget};
use crate::data::{Player, Snapshot};
use crate::memory;

/// 死区（像素）：已经贴住准心就别再动鼠标 —— 治抖动最关键的一条
const DEAD_ZONE_PX: f32 = 4.0;
/// 单帧最大鼠标位移（计数）
const MAX_STEP: f32 = 36.0;
/// 每像素计数 的初始猜测（×1000，即 0.3）
const K_INIT: i32 = 300;
/// 离自己这么近的目标不瞄（重复的自己 / 载具里的自己）。
/// ⚠️ 原来 2.0 米：香肠派对的人机常常是贴脸距离，整批人机会被这条一刀切掉
/// （表现就是「人机就在准心上，准心一点不动」）。自己已经由 ptr 比对排除了，这里只兜底重复对象。
const MIN_TARGET_DIST: f32 = 0.6;

/// 每像素需要多少鼠标计数（×1000），自适应学习
static KX: AtomicI32 = AtomicI32::new(K_INIT);
static KY: AtomicI32 = AtomicI32::new(K_INIT);
/// 上一帧的误差（像素 ×100）和实际发出的位移（计数），用来反推 K
static PREV_EX: AtomicI32 = AtomicI32::new(0);
static PREV_EY: AtomicI32 = AtomicI32::new(0);
static PREV_MX: AtomicI32 = AtomicI32::new(0);
static PREV_MY: AtomicI32 = AtomicI32::new(0);
static HAVE_PREV: AtomicBool = AtomicBool::new(false);
/// 上一帧锁定的目标实体指针
static STICKY: AtomicUsize = AtomicUsize::new(0);

/// 目标屏幕坐标的指数平滑（EMA）。直接用每帧原始投影坐标算误差时，
/// 坐标本身（脚/头的高度去抖、姿态缩放、矩阵浮点）每帧都在小幅跳，准心就会跟着抖。
/// 平滑一下就不抖了；换目标时立即重置，不拖影。
static AIM_SX: AtomicU32 = AtomicU32::new(0);
static AIM_SY: AtomicU32 = AtomicU32::new(0);
static AIM_PTR: AtomicUsize = AtomicUsize::new(0);

fn smooth_aim_point(ptr: usize, x: f32, y: f32, alpha: f32) -> (f32, f32) {
    if AIM_PTR.load(Ordering::Relaxed) != ptr {
        AIM_PTR.store(ptr, Ordering::Relaxed);
        AIM_SX.store(x.to_bits(), Ordering::Relaxed);
        AIM_SY.store(y.to_bits(), Ordering::Relaxed);
        return (x, y);
    }
    let px = f32::from_bits(AIM_SX.load(Ordering::Relaxed));
    let py = f32::from_bits(AIM_SY.load(Ordering::Relaxed));
    let nx = px + (x - px) * alpha;
    let ny = py + (y - py) * alpha;
    AIM_SX.store(nx.to_bits(), Ordering::Relaxed);
    AIM_SY.store(ny.to_bits(), Ordering::Relaxed);
    (nx, ny)
}

/// 目标屏幕速度（像素/帧，EMA）+ 超前补偿。
/// 恒定速度目标下，EMA 平滑 + “只走一部分误差”的鼠标闭环会有稳态滞后，
/// 结果就是“自喵老是落在移动敌人身后”。这里把目标速度乘一个可调帧数补上去。
static TGT_VX: AtomicU32 = AtomicU32::new(0);
static TGT_VY: AtomicU32 = AtomicU32::new(0);
static TGT_PX: AtomicU32 = AtomicU32::new(0);
static TGT_PY: AtomicU32 = AtomicU32::new(0);
static TGT_PTR_S: AtomicUsize = AtomicUsize::new(0);

/// 返回“超前量”（像素）：目标速度 × 补偿帧数。换目标自动归零。
fn screen_lead(ptr: usize, x: f32, y: f32, lead_frames: f32) -> (f32, f32) {
    let px = f32::from_bits(TGT_PX.load(Ordering::Relaxed));
    let py = f32::from_bits(TGT_PY.load(Ordering::Relaxed));
    let mut vx = 0.0f32;
    let mut vy = 0.0f32;
    if TGT_PTR_S.load(Ordering::Relaxed) == ptr {
        let cap = MAX_STEP * 4.0;
        let dx = (x - px).clamp(-cap, cap);
        let dy = (y - py).clamp(-cap, cap);
        vx = f32::from_bits(TGT_VX.load(Ordering::Relaxed)) * 0.6 + dx * 0.4;
        vy = f32::from_bits(TGT_VY.load(Ordering::Relaxed)) * 0.6 + dy * 0.4;
    }
    TGT_PTR_S.store(ptr, Ordering::Relaxed);
    TGT_PX.store(x.to_bits(), Ordering::Relaxed);
    TGT_PY.store(y.to_bits(), Ordering::Relaxed);
    TGT_VX.store(vx.to_bits(), Ordering::Relaxed);
    TGT_VY.store(vy.to_bits(), Ordering::Relaxed);
    let l = lead_frames.clamp(0.0, 8.0);
    (vx * l, vy * l)
}

pub fn key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

fn move_mouse(dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    let inp = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(1, &inp, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 屏幕正中画自瞄范围（FOV 圈）+ 准心，开关打开时才画。
fn draw_overlay(ui: &imgui::Ui, config: &Config, snapshot: &Snapshot) {
    let [sw, sh] = ui.io().display_size;
    if sw <= 1.0 || sh <= 1.0 {
        return;
    }
    let dl = ui.get_foreground_draw_list();
    let c = [sw * 0.5, sh * 0.5];
    let col = [0.25, 1.0, 0.45, 0.85];
    dl.add_circle(c, config.aim_fov, col).thickness(1.5).build();
    dl.add_circle(c, 2.5, [1.0, 1.0, 1.0, 0.7]).thickness(1.0).build();
    dl.add_line([c[0] - 9.0, c[1]], [c[0] + 9.0, c[1]], col)
        .thickness(1.0)
        .build();
    dl.add_line([c[0], c[1] - 9.0], [c[0], c[1] + 9.0], col)
        .thickness(1.0)
        .build();

    // 目标读数：直接看清“真头”有没有生效、坐标有没有变（探头时看 x/z 变不变）。
    let sp = STICKY.load(Ordering::Relaxed);
    if sp != 0 {
        if let Some(p) = snapshot.players.iter().find(|p| p.ptr == sp) {
            let dy = p
                .head
                .map(|h| format!("{:.2}", h.y - p.feet.y))
                .unwrap_or_else(|| "-".into());
            let hs = match p.head {
                Some(h) => format!("({:.2},{:.2},{:.2})", h.x, h.y, h.z),
                None => "无(脚+身高)".into(),
            };
            let real = if p.head.is_some() { "真头" } else { "回退" };
            let lines = [
                format!("目标: {}", p.name_str()),
                format!("头源: {real}  dY={dy}"),
                format!("脚=({:.2},{:.2},{:.2})", p.feet.x, p.feet.y, p.feet.z),
                format!("头={hs}"),
                format!("侧身={}  姿态={}  朝向={:.0}°", p.side, p.stance_enum, p.yaw.to_degrees()),
                format!(
                    "速度=({:.1},{:.1},{:.1})m/s",
                    p.vel.x, p.vel.y, p.vel.z
                ),
            ];
            for (i, l) in lines.iter().enumerate() {
                dl.add_text([c[0] + config.aim_fov + 10.0, c[1] - 40.0 + i as f32 * 15.0], col, l);
            }
        }
    }
}

/// 这个实体要瞄的世界坐标。
/// 关键：和**框顶完全一致** —— 先算“脚 + 身高（或真实头）”，再按姿态比例压矮
/// （不然蹲下时框变矮了、瞄点还停在原来高度，就会跑到框上方）。
/// 再加上：自定义上下偏移 + 预判（提前量）。
fn target_world(config: &Config, _snapshot: &Snapshot, p: &Player) -> memory::Vec3 {
    // 真头（headPoint）本身就已经跟着蹲下/趴下/探头到位了，不能再乘姿态比例。
    let head_is_real = config.aim_target == SnapTarget::Head && p.head.is_some();
    // 无真头（合成人机）→ 用「脚 + 身高」估头顶，**也必须叠水平补偿**。
    // 原来只在 `p.head` 为 Some 时加 lean_shift：人机一探头/趴下，模型的头已经挪出去，
    // 瞄点却还钉在身体中轴上 → 表现就是「人机站着能打头，一探头就打不到」。
    let base = match config.aim_target {
        SnapTarget::Head => {
            let mut h = p
                .head
                .unwrap_or_else(|| p.feet.offset_up(config.player_height));
            let (dx, dz) = crate::data::lean_shift(config, p);
            h.x += dx;
            h.z += dz;
            h
        }
        SnapTarget::Feet => p.feet,
    };

    // 和框高一致（脚不动，只压上端）：`p.stance` 现在是身高比例（1.0/0.65/0.30）
    let mut top = if !head_is_real && config.crouch_follow {
        crate::memory::Vec3 {
            x: base.x,
            y: p.feet.y + config.player_height * p.stance.clamp(0.15, 1.0),
            z: base.z,
        }
    } else {
        base
    };

    // 自定义瞄点上下移
    top.y += config.aim_offset_y;

    // 预判：提前量（秒），固定用滑块值
    let lead = config.aim_lead_time.clamp(0.0, 0.6);
    if lead > 0.0 {
        // 预判位移封顶 2.5m，避免“飞过头”
        let mut lx = p.vel.x * lead;
        let mut lz = p.vel.z * lead;
        let lm = (lx * lx + lz * lz).sqrt();
        if lm > 2.5 {
            let s = 2.5 / lm;
            lx *= s;
            lz *= s;
        }
        top.x += lx;
        top.z += lz;
    }

    top
}

fn usable(config: &Config, snapshot: &Snapshot, p: &Player) -> bool {
    if !p.alive {
        return false;
    }
    // 只瞄敌人：队友排除。
    // 人机（AI）必须单独放行 —— data.rs 里 AI 的 enemy 恒为 false（人机不走队伍阵营判定），
    // 老逻辑「只要 !enemy 就扔」会把整个人机局的目标全部排除 → 人机模式自瞄完全没有目标。
    // 不按队伍号滤人机：日志实测 TeamNum 有 0/1/2，0 是合法队号，AI 的 TeamId 常和真人同号，
    // 按队伍判会把敌人机一起滤掉（静默失灵）—— 所以「瞄人机」开着就一律放行。
    if config.aim_enemy_only && !p.enemy && !(config.aim_bots && p.bot) {
        return false;
    }
    if config.ignore_self && snapshot.self_ptr != 0 && p.ptr == snapshot.self_ptr {
        return false;
    }
    if let Some(sp) = snapshot.self_pos {
        let d = ((p.feet.x - sp.x).powi(2) + (p.feet.z - sp.z).powi(2)).sqrt();
        if d < MIN_TARGET_DIST {
            return false;
        }
    }
    true
}

/// 自瞄诊断日志：限频 1 秒一条，把“为什么没瞄”写清楚。
static AIM_LOG_MS: AtomicU64 = AtomicU64::new(0);

/// 到了限频窗口就返回 true（并消耗掉这个窗口）。
fn aim_due() -> bool {
    let now = t_ms();
    if now.saturating_sub(AIM_LOG_MS.load(Ordering::Relaxed)) < 1000 {
        return false;
    }
    AIM_LOG_MS.store(now, Ordering::Relaxed);
    true
}

fn aim_note(msg: &str) {
    crate::log::append(&format!("[自瞄] {msg}\n"));
}

/// 每帧调用一次。
pub fn run(ui: &imgui::Ui, config: &Config, snapshot: &Snapshot) {
    if !config.aim_enabled {
        if aim_due() {
            aim_note("没效果：总开关『自瞄』没勾");
        }
        return;
    }
    draw_overlay(ui, config, snapshot);

    if !snapshot.valid {
        if aim_due() {
            aim_note("暂停：快照无效（矩阵/数据未就绪）");
        }
        return;
    }
    if config.menu_open {
        if aim_due() {
            aim_note("暂停：菜单开着（自瞄在菜单打开时一律不工作）");
        }
        return;
    }
    if ui.io().want_capture_mouse {
        if aim_due() {
            aim_note("暂停：鼠标被窗口捕获");
        }
        return;
    }
    if config.aim_hold_mode && !key_down(config.aim_key) {
        if aim_due() {
            aim_note(&format!(
                "暂停：『按键触发自喵』开着，未按住 {}",
                vk_name(config.aim_key)
            ));
        }
        return;
    }

    let [sw, sh] = ui.io().display_size;
    if sw <= 1.0 || sh <= 1.0 {
        return;
    }
    let cx = sw * 0.5;
    let cy = sh * 0.5;
    let proj = config.projection();

    let screen_of = |p: &Player| -> Option<memory::Vec2> {
        let world = target_world(config, snapshot, p);
        memory::world_to_screen(world, snapshot.view_matrix, sw, sh, proj)
    };
    let in_fov = |s: memory::Vec2| -> Option<f32> {
        let d = ((s.x - cx).powi(2) + (s.y - cy).powi(2)).sqrt();
        if d <= config.aim_fov {
            Some(d)
        } else {
            None
        }
    };
    // 身体中心投影：**只用于「能不能入选 FOV」的判定**。
    // 为什么单独要这一份：真人走真头（headPoint），瞄点准；而合成人机（只有 RoleAILogic、
    // 没有 BattleRoleLogic）拿不到真头，瞄点只能按「脚 + player_height」估。
    // 这 1.5 米一旦估得不准（人机模型身高/Position 基准和真人不同），瞄点会整个掉出 FOV，
    // 表现就是「人机明明在准心上、准心却一点不动」。入选改用身体中心后，估高偏差只影响
    // 最终误差大小，不再决定能不能锁上。
    let center_screen_of = |p: &Player| -> Option<memory::Vec2> {
        let top = p
            .head
            .unwrap_or_else(|| p.feet.offset_up(config.player_height));
        let c = memory::Vec3 {
            x: (p.feet.x + top.x) * 0.5,
            y: (p.feet.y + top.y) * 0.5,
            z: (p.feet.z + top.z) * 0.5,
        };
        memory::world_to_screen(c, snapshot.view_matrix, sw, sh, proj)
    };

    // ① 先算出“离准心最近的那个”（永远要有这个作为基准）
    let mut best: Option<(f32, memory::Vec2, usize)> = None;
    let n_total = snapshot.players.len();
    let mut n_usable = 0usize;
    let mut n_fov = 0usize;
    let mut n_ai_blocked = 0usize;
    for p in snapshot.players.iter() {
        if !usable(config, snapshot, p) {
            if p.bot {
                n_ai_blocked += 1;
            }
            continue;
        }
        n_usable += 1;
        let Some(s) = screen_of(p) else { continue };
        // 入选优先看「身体中心」，它不在 FOV 时才退回瞄点。
        // （真人贴脸时头在 FOV 内、身体中心可能刚好在外，所以两条都要看）
        let Some(d) = center_screen_of(p).and_then(in_fov).or_else(|| in_fov(s)) else {
            continue;
        };
        n_fov += 1;
        if best.as_ref().map_or(true, |(bd, _, _)| d < *bd) {
            best = Some((d, s, p.ptr));
        }
    }

    // ② 上一帧锁定的目标还在视野里的话：只有当它离准心不比“最近的那个”远超过
    //    aim_stick_px 像素时才继续用它（= 不横跳）；阈值设 0 就是“永远瞄离准心最近的”。
    let mut sticky_cand: Option<(f32, memory::Vec2, usize)> = None;
    let sticky = STICKY.load(Ordering::Relaxed);
    if sticky != 0 {
        if let Some(p) = snapshot.players.iter().find(|p| p.ptr == sticky) {
            if usable(config, snapshot, p) {
                if let Some(s) = screen_of(p) {
                    if let Some(d) = center_screen_of(p).and_then(in_fov).or_else(|| in_fov(s)) {
                        sticky_cand = Some((d, s, p.ptr));
                    }
                }
            }
        }
    }
    let keep_sticky = match (sticky_cand.as_ref(), best.as_ref()) {
        (Some(sd), Some(bd)) => sd.0 <= bd.0 + config.aim_stick_px,
        (Some(_), None) => true,
        _ => false,
    };
    let chosen = if keep_sticky { sticky_cand } else { best };

    let Some((_raw_dist, raw_target, ptr)) = chosen else {
        HAVE_PREV.store(false, Ordering::Relaxed);
        AIM_PTR.store(0, Ordering::Relaxed);
        if aim_due() {
            let hint = if n_ai_blocked > 0 && !config.aim_bots {
                "（有 AI 被『瞄人机』开关关掉了 → 人机模式把它打开）"
            } else if n_usable == 0 {
                "（可用=0：多半是『只瞄敌人』把队友和 AI 全排除了；人机局请勾上『瞄人机』）"
            } else {
                ""
            };
            // 人机逐个体检：合成人机（只有 RoleAILogic、没有 BattleRoleLogic）在 data.rs 里
            // enemy 恒 false、坐标走 RoleAILogic.Position（服务端同步）。瞄不上时，
            // 必须一眼看出是被 alive / 敌我 / 距离 / 投影 / FOV 里的哪一道挡的。
            let mut extra = String::new();
            let bots: Vec<&Player> = snapshot.players.iter().filter(|p| p.bot).collect();
            if !bots.is_empty() {
                extra.push_str(&format!("  [人机体检] 列表内人机={}", bots.len()));
                for p in bots.iter().take(4) {
                    let d = snapshot.self_pos.map_or(-1.0, |sp| {
                        ((p.feet.x - sp.x).powi(2) + (p.feet.z - sp.z).powi(2)).sqrt()
                    });
                    let proj = screen_of(p);
                    let spos = proj.map_or("投影失败".to_string(), |s| format!("({:.0},{:.0})", s.x, s.y));
                    let fov = proj
                        .and_then(&in_fov)
                        .map_or("FOV外".to_string(), |x| format!("{x:.0}px"));
                    extra.push_str(&format!(
                        "\n      {} ptr=0x{:X} 距={d:.1}m 屏幕={spos} {fov} 可用={} 敌={} 活={}",
                        p.name_str(),
                        p.ptr,
                        usable(config, snapshot, p),
                        p.enemy,
                        p.alive
                    ));
                }
            }
            aim_note(&format!(
                "没目标：玩家={n_total} 可用={n_usable} FOV内={n_fov} 被排除的AI={n_ai_blocked}{hint}{extra}"
            ));
        }
        return;
    };
    STICKY.store(ptr, Ordering::Relaxed);


    // 屏幕坐标平滑：smooth 越大越平滑（同时比例增益也越小），治“准心在目标周围抖”
    let alpha = (1.0 / (1.0 + config.aim_smooth.max(0.0))).clamp(0.15, 1.0);
    let (sx, sy) = smooth_aim_point(ptr, raw_target.x, raw_target.y, alpha);
    // 超前补偿：治“自喵落在移动敌人身后”（用原始坐标算速度，叠到平滑后的瞄点上）
    let (lx, ly) = screen_lead(ptr, raw_target.x, raw_target.y, config.aim_screen_lead);
    let (ax, ay) = (sx + lx, sy + ly);
    let dist = ((ax - cx).powi(2) + (ay - cy).powi(2)).sqrt();

    // 死区：已经贴住准心就不动鼠标
    if dist < DEAD_ZONE_PX {
        HAVE_PREV.store(false, Ordering::Relaxed);
        if aim_due() {
            aim_note(&format!(
                "已贴住准心（死区 <{DEAD_ZONE_PX:.0}px），不动鼠标；误差={dist:.1}px"
            ));
        }
        return;
    }

    let ex = ax - cx;
    let ey = ay - cy;

    //  —— 自适应：用上一帧「发了多少 / 误差减少多少」反推每像素需要多少计数 ——
    // 关键：只在误差明显减小时学习，且学习率调低、K 夹在窄范围内。
    // 原先把 K 放到 0.02~20、每帧 EMA 0.3，K 自己会震荡 → 准心跟着抖。
    let mut kx = KX.load(Ordering::Relaxed) as f32 / 1000.0;
    let mut ky = KY.load(Ordering::Relaxed) as f32 / 1000.0;
    if HAVE_PREV.load(Ordering::Relaxed) {
        let pmx = PREV_MX.load(Ordering::Relaxed) as f32;
        let pmy = PREV_MY.load(Ordering::Relaxed) as f32;
        let pex = PREV_EX.load(Ordering::Relaxed) as f32 / 100.0;
        let pey = PREV_EY.load(Ordering::Relaxed) as f32 / 100.0;
        let dxs = pex - ex; // 误差减少量（和位移同号才有效）
        let dys = pey - ey;
        if pmx.abs() >= 2.0 && dxs.abs() > 2.0 && pmx * dxs > 0.0 {
            let est = (pmx / dxs).clamp(0.05, 1.5);
            kx = (kx * 0.88 + est * 0.12).clamp(0.05, 1.5);
            KX.store((kx * 1000.0) as i32, Ordering::Relaxed);
        }
        if pmy.abs() >= 2.0 && dys.abs() > 2.0 && pmy * dys > 0.0 {
            let est = (pmy / dys).clamp(0.05, 1.5);
            ky = (ky * 0.88 + est * 0.12).clamp(0.05, 1.5);
            KY.store((ky * 1000.0) as i32, Ordering::Relaxed);
        }
    }

    // 只走剩余误差的一部分（frac < 1 → 只会收敛不会振荡），并且封顶 0.5，
    // 保证任何灵敏度下都不会“冲过头 → 反向 → 再来”地摆。
    let frac = (0.55 / config.aim_smooth.max(1.0)).min(0.5);
    let speed = config.aim_speed.max(0.1);
    let mx = (ex * frac * kx * speed).clamp(-MAX_STEP, MAX_STEP).round() as i32;
    let my = (ey * frac * ky * speed).clamp(-MAX_STEP, MAX_STEP).round() as i32;

    if aim_due() {
        if let Some(tp) = snapshot.players.iter().find(|p| p.ptr == ptr) {
            // [人机] 标记：一眼分清这轮锁的是人机还是真人，便于对比两边的行为差异。
            let tag = if tp.bot { "[人机] " } else { "" };
            let d = snapshot.self_pos.map_or(-1.0, |sp| {
                ((tp.feet.x - sp.x).powi(2) + (tp.feet.z - sp.z).powi(2)).sqrt()
            });
            aim_note(&format!(
                "瞄 {tag}{}: 距={d:.1}m 可用={n_usable} FOV内={n_fov} 误差=({ex:.0},{ey:.0})px 位移=({mx},{my})",
                tp.name_str()
            ));
        }
    }
    if mx == 0 && my == 0 {
        if aim_due() {
            aim_note(&format!("误差很小 误差=({ex:.0},{ey:.0})px → 位移取整为 0"));
        }
        return;
    }
    PREV_EX.store((ex * 100.0) as i32, Ordering::Relaxed);
    PREV_EY.store((ey * 100.0) as i32, Ordering::Relaxed);
    PREV_MX.store(mx, Ordering::Relaxed);
    PREV_MY.store(my, Ordering::Relaxed);
    HAVE_PREV.store(true, Ordering::Relaxed);
    move_mouse(mx, my);
}


// ===================== 自瞄按键绑定（可自定义） =====================
static BIND_ARM_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn t_ms() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// 菜单点「按键」后进入"等待按键"状态。
pub fn begin_bind_key() {
    BIND_ARM_MS.store(t_ms().max(1), std::sync::atomic::Ordering::Relaxed);
}

/// 是否正在等待按键。
pub fn binding() -> bool {
    BIND_ARM_MS.load(std::sync::atomic::Ordering::Relaxed) != 0
}

/// 渲染线程每帧调：等键时捕获下一个按下的键并返回（Esc = 取消）。
/// 点完按钮后的 600ms 内忽略鼠标左键，免得立刻绑到"点按钮"那一下。
pub fn poll_bind_key() -> Option<i32> {
    use std::sync::atomic::Ordering;
    let arm = BIND_ARM_MS.load(Ordering::Relaxed);
    if arm == 0 {
        return None;
    }
    let fresh = t_ms().saturating_sub(arm) < 600;
    for vk in 1..=254i32 {
        if !key_down(vk) {
            continue;
        }
        if vk == 27 {
            BIND_ARM_MS.store(0, Ordering::Relaxed);
            return None;
        }
        if vk == 1 && fresh {
            continue;
        }
        BIND_ARM_MS.store(0, Ordering::Relaxed);
        return Some(vk);
    }
    None
}

/// VK 码 → 好读的名字
pub fn vk_name(vk: i32) -> String {
    match vk {
        1 => "鼠标左键".into(),
        2 => "鼠标右键".into(),
        4 => "鼠标中键".into(),
        5 => "鼠标侧键1".into(),
        6 => "鼠标侧键2".into(),
        9 => "Tab".into(),
        16 => "Shift".into(),
        17 => "Ctrl".into(),
        18 => "Alt".into(),
        20 => "CapsLock".into(),
        32 => "空格".into(),
        0x30..=0x39 => ((vk as u8) as char).to_string(),
        0x41..=0x5A => ((vk as u8) as char).to_string(),
        _ => format!("VK 0x{vk:02X}"),
    }
}
