use imgui::Ui;

use crate::config::Config;
use crate::data::{self, Snapshot};
use crate::log;
use crate::memory;
use crate::memory::{game_client_size, Projection, Vec3};

const BOX_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const LINE_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

pub fn draw(ui: &Ui, config: &Config, snapshot: &Snapshot) {
    // 自瞄先跑：不能挂在 ESP 开关后面（ESP 一关/拿不到客户区尺寸，draw 会提前 return，
    // 自瞄就会被连坐）。自瞄自己用 display_size，不依赖 game_client_size。
    crate::aim::run(ui, config, snapshot);

    if !config.esp_enabled {
        return;
    }

    let Some((screen_w, screen_h)) = game_client_size() else {
        log::stage("ESP: 拿不到游戏客户区尺寸(hwnd 或尺寸为 0)");
        return;
    };

    let proj = config.projection();
    let matrix = snapshot.view_matrix;

    // 手动地址优先：直接用 CE 里找到的地址，方便验证是不是玩家。
    if let Some(addr) = config.manual_addr_value() {
        if let Some(feet) = data::read_position(addr) {
            draw_one(
                ui, config, feet, None, None, None, false, matrix, screen_w, screen_h, proj,
                snapshot.self_pos,
                0.0,
                false,
                true,
                "",
                0.0,
                0.0,
            );
        }
        return;
    }

    if !snapshot.valid {
        // 具体原因 data.rs 已经写进日志了，这里不重复刷
        return;
    }

    // 调试：把本地玩家(列表第 0 项)用洋红色框标出来。
    // 屏幕上你自己的角色如果在 SELF 框里 → 这个矩阵就是主渲染相机；
    // 如果 SELF 框跑到别处去 → 矩阵不是主相机（比如俯视相机）。
    // 自己的框（菜单里可开），免得自己身上也挂一个框
    if config.draw_self_box {
        draw_self(ui, config, snapshot.self_ptr, matrix, screen_w, screen_h, proj);
    }

    // 相机朝向（水平方向）：顶部统计里判断“敌人是不是在背后”用。
    // 第三人称相机基本就是沿着“自己 - 相机”的方向看，所以这个向量够用。
    let fwd = match (snapshot.self_pos, memory::camera_position(matrix)) {
        (Some(sp), Some(c)) => {
            let (fx, fz) = (sp.x - c.x, sp.z - c.z);
            if fx * fx + fz * fz > 0.01 {
                Some((fx, fz))
            } else {
                None
            }
        }
        _ => None,
    };

    let mut drawn = 0usize;
    let mut enemy_drawn = 0usize;
    let mut bots_drawn = 0usize;
    let mut behind = 0usize;
    for (i, p) in snapshot.players.iter().enumerate() {
        if !p.alive {
            continue;
        }
        // 统计：在背后（自己-相机方向的另一侧）= 背敌
        if let (Some(sp), Some((fx, fz))) = (snapshot.self_pos, fwd) {
            if (p.feet.x - sp.x) * fx + (p.feet.z - sp.z) * fz < 0.0 {
                behind += 1;
            }
        }
        // 统计：投影落在屏幕内 = 真正画出来了
        if let Some(s) = memory::world_to_screen(p.feet, matrix, screen_w, screen_h, proj) {
            if s.x > -60.0 && s.x < screen_w + 60.0 && s.y > -60.0 && s.y < screen_h + 60.0 {
                drawn += 1;
                if p.bot {
                    bots_drawn += 1;
                }
                if p.enemy {
                    enemy_drawn += 1;
                }
            }
        }
        let (lean_dx, lean_dz) = data::lean_shift(config, p);
        draw_one(
            ui,
            config,
            p.feet,
            p.head,
            Some(i),
            p.health,
            p.stale,
            matrix,
            screen_w,
            screen_h,
            proj,
            snapshot.self_pos,
            p.stance,
            p.bot,
            p.enemy,
            p.name_str(),
            lean_dx,
            lean_dz,
        );
    }

    // 顶部统计条
    if config.draw_counter {
        let text = format!(
        "绘制 {} 人 (敌 {} / 友 {} / AI {})    背敌 {} 人",
        drawn,
        enemy_drawn,
        drawn - enemy_drawn - bots_drawn,
        bots_drawn,
        behind
    );
        ui.get_foreground_draw_list()
            .add_text(
                [screen_w * 0.5 - 78.0, 70.0],
                [1.0, 1.0, 0.35, 1.0],
                &text,
            );
    }

    // 统计日志（1 秒一条）：方便远程排查“背敌/绘制”是否正常
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static ESP_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if now.saturating_sub(ESP_LOG_MS.load(Ordering::Relaxed)) >= 1000 {
            ESP_LOG_MS.store(now, Ordering::Relaxed);
            log::append(&format!(
                "[统计] 总玩家={} 绘制={drawn} 敌={enemy_drawn} AI={bots_drawn} 背敌={behind}\n",
                snapshot.players.len()
            ));
        }
    }
}

fn draw_one(
    ui: &Ui,
    config: &Config,
    feet_world: Vec3,
    real_head: Option<Vec3>,
    _idx: Option<usize>,
    health: Option<i32>,
    stale: bool,
    matrix: [f32; 16],
    screen_w: f32,
    screen_h: f32,
    proj: Projection,
    self_pos: Option<Vec3>,
    stance: f32,
    bot: bool,
    enemy: bool,
    name: &str,
    lean_dx: f32,
    lean_dz: f32,
) {
    // 上一帧的位置（读到了异常值）= 灰色，和正常框区分开
    let (box_color, line_color) = if stale {
        ([0.6, 0.6, 0.6, 1.0], [0.6, 0.6, 0.6, 1.0])
    } else if config.team_check {
        if bot {
            ([1.0, 0.85, 0.20, 1.0], [1.0, 0.85, 0.20, 1.0]) // 人机 = 黄
        } else if enemy {
            ([1.0, 0.30, 0.30, 1.0], [1.0, 0.30, 0.30, 1.0]) // 敌人 = 红
        } else {
            ([0.35, 0.95, 1.0, 1.0], [0.35, 0.95, 1.0, 1.0]) // 队友 = 青
        }
    } else {
        (BOX_COLOR, LINE_COLOR)
    };
    // 侧身/探头补偿：把头顶往探头方向平移（SyncSideAim × 朝向右向量 × 距离）
    let real_head = real_head.map(|mut h| {
        h.x += lean_dx;
        h.z += lean_dz;
        h
    });
    // 头顶：优先用内存里读到的真实头（蹲下会跟着降）。
    let head_is_real = real_head.is_some();
    let head_world = real_head
        .map(|h| h.offset_up(config.head_pad))
        .unwrap_or_else(|| feet_world.offset_up(config.player_height));

    // 蹲下/趴下跟随：
    // `stance` 现在是“身高比例”（站立 1.0 / 蹲下 0.65 / 趴下 0.30，见 data::stance_height_scale）。
    // ⚠️ 只有【回退身高】时才用它 —— 真头 headPoint 本身已经跟着姿态到位了，再乘一次会双重压低。
    let head_world = if !head_is_real && config.crouch_follow {
        Vec3 {
            x: head_world.x,
            y: feet_world.y + config.player_height * stance.clamp(0.15, 1.0),
            z: head_world.z,
        }
    } else {
        head_world
    };

    let Some(head) = memory::world_to_screen(head_world, matrix, screen_w, screen_h, proj) else {
        return;
    };
    let Some(feet) = memory::world_to_screen(feet_world, matrix, screen_w, screen_h, proj) else {
        return;
    };

    // 总校验：投影结果非法/超大（坏矩阵 / 旧链垃圾实体）→ 直接跳过这个人，
    // 否则 snapline/骨骼/昵称 拿到 NaN 或 1e30 会把 ImGui/D3D 弄崩。
    let bad = |v: f32| !v.is_finite() || v.abs() > 100_000.0;
    if bad(head.x) || bad(head.y) || bad(feet.x) || bad(feet.y) {
        return;
    }

    // 框：默认用「投影高度 × 宽高比」算 —— 稳定、不会被透视/贴近相机拉宽，
    // x 取头脚中点，所以始终贴在身体上（自瞄瞄的也是这个头）。
    // 只有勾上「框用世界四角(旧)」才走旧的四角 AABB（近距离看着会很宽）。
    let height_px = (feet.y - head.y).abs().max(1.0);
    let ratio = (config.box_width / config.player_height.max(0.2)).clamp(0.15, 0.8);
    let (box_min, box_max) = ratio_box(head, feet, height_px, ratio, config.box_pad);

    if config.draw_snapline {
        ui.get_foreground_draw_list()
            .add_line([screen_w * 0.5, 0.0], head.to_array(), line_color)
            .thickness(1.0)
            .build();
    }

    // 防御：坐标非法/过大（矩阵坏时可能算出 1e30）→ 不画，避免 ImGui/D3D 闪退
    let sane = |v: f32| v.is_finite() && v.abs() < 100_000.0;
    if !(sane(box_min[0]) && sane(box_min[1]) && sane(box_max[0]) && sane(box_max[1])) {
        return;
    }

    if config.draw_box_enabled {
        ui.get_foreground_draw_list()
            .add_rect(box_min, box_max, box_color)
            .thickness(1.0)
            .build();
    }

    // 昵称（框上方居中）
    if config.draw_name && !name.is_empty() {
        let w_est = name.chars().count() as f32 * 7.0;
        ui.get_foreground_draw_list().add_text(
            [head.x - w_est * 0.5, box_min[1] - 16.0],
            [1.0, 1.0, 1.0, 1.0],
            name,
        );
    }

    // 骨架（开关在菜单里）
    if config.draw_skeleton {
        draw_stick(ui, head, feet, [0.25, 1.0, 0.45, 1.0]);
    }

    if config.draw_health_bar {
        if let Some(hp) = health {
            let frac = ((hp as f32) / 100.0).clamp(0.0, 1.0);
            let dl = ui.get_foreground_draw_list();
            let bx = [box_min[0] - 7.0, box_min[1]];
            let bx2 = [box_min[0] - 3.0, box_max[1]];
            // 背景
            dl.add_rect(bx, bx2, [0.0, 0.0, 0.0, 0.65])
                .filled(true)
                .build();
            // 前景（从下往上长）
            let h = (bx2[1] - bx[1]) * frac;
            let col = if frac > 0.6 {
                [0.20, 1.0, 0.30, 1.0]
            } else if frac > 0.3 {
                [1.0, 0.85, 0.15, 1.0]
            } else {
                [1.0, 0.25, 0.25, 1.0]
            };
            dl.add_rect([bx[0], bx2[1] - h], [bx2[0], bx2[1]], col)
                .filled(true)
                .build();
            // 数字
            dl.add_text(
                [box_min[0] - 34.0, (box_min[1] + box_max[1]) * 0.5],
                [1.0; 4],
                format!("{hp}"),
            );
        }
    }

    // 距离标签（离自己多少米）
    if config.draw_name {
        let dist = self_pos
            .map(|m| {
                let dx = feet_world.x - m.x;
                let dy = feet_world.y - m.y;
                let dz = feet_world.z - m.z;
                (dx * dx + dy * dy + dz * dz).sqrt()
            })
            .unwrap_or(0.0);
        let text = format!("{dist:.0}m");
        ui.get_foreground_draw_list().add_text(
            [box_min[0], box_max[1] + 2.0],
            [1.0, 1.0, 0.0, 1.0],
            &text,
        );
    }
}

/// 动态世界框：以 `ref_pos`（自己）→目标的水平方向为视线，取人物左右各 `width_m/2`，
/// 把 4 个世界角点投影到屏幕后取 AABB。这样宽度随距离透视缩放、并跟随横向位置。
/// 按「投影高度 × 宽高比」画框，x 取头/脚中点。
/// 比“世界四角投影取 AABB”稳定得多：贴近相机 / 透视极端时不会被拉宽。
fn ratio_box(
    head: memory::Vec2,
    feet: memory::Vec2,
    height_px: f32,
    ratio: f32,
    pad: f32,
) -> ([f32; 2], [f32; 2]) {
    let w = (height_px * ratio).max(8.0);
    let cx = (head.x + feet.x) * 0.5;
    (
        [cx - w * 0.5 - pad, head.y.min(feet.y) - pad],
        [cx + w * 0.5 + pad, head.y.max(feet.y) + pad],
    )
}

/// 近似骨架（棍人）：只靠“脚 + 头顶”两个点按比例推算，
/// 所以它只能跟着蹲下/跳起整体缩放，不会跟着手臂/腿的真实姿态动。
/// （真实骨骼坐标要先在内存里找到骨骼数组，那是另一件事。）
fn draw_stick(ui: &Ui, head: memory::Vec2, feet: memory::Vec2, color: [f32; 4]) {
    let h = feet.y - head.y;
    if !(h > 10.0) {
        return;
    }
    let w = h * 0.5;
    let cx = head.x;
    let dl = ui.get_foreground_draw_list();

    let p = |fx: f32, fy: f32| memory::Vec2 {
        x: cx + fx * w,
        y: head.y + fy * h,
    };
    let head_c = p(0.0, 0.13);
    let neck = p(0.0, 0.27);
    let pelvis = p(0.0, 0.52);
    let hand_l = p(-0.32, 0.46);
    let hand_r = p(0.32, 0.46);
    let foot_l = p(-0.17, 1.0);
    let foot_r = p(0.17, 1.0);

    let seg = |a: memory::Vec2, b: memory::Vec2| {
        dl.add_line(a.to_array(), b.to_array(), color)
            .thickness(1.5)
            .build();
    };
    seg(neck, pelvis);
    seg(pelvis, foot_l);
    seg(pelvis, foot_r);
    seg(neck, hand_l);
    seg(neck, hand_r);
    seg(head_c, neck);
    dl.add_circle(head_c.to_array(), h * 0.10, color)
        .thickness(1.5)
        .build();
}

/// 读本地玩家的实体指针 + 世界坐标（实时位置 + 去抖动，跳跃会跟随）。
/// 现在“自己”由 data 层按实体指针认定（列表第 0 项不一定是自己了）。
fn self_entity(self_ptr: usize) -> Option<(usize, Vec3)> {
    if self_ptr < 0x10000 {
        return None;
    }
    data::read_draw_pos(self_ptr, None).map(|p| (self_ptr, p))
}

/// 调试：洋红色框 + 标签标出“自己”，用来验证矩阵是不是主渲染相机。
fn draw_self(
    ui: &Ui,
    config: &Config,
    self_ptr: usize,
    matrix: [f32; 16],
    screen_w: f32,
    screen_h: f32,
    proj: Projection,
) {
    let Some((sp_ptr, sp)) = self_entity(self_ptr) else {
        return;
    };
    let real_head = data::read_role_head(sp_ptr);
    let base_head = real_head
        .map(|h| h.offset_up(config.head_pad))
        .unwrap_or_else(|| sp.offset_up(config.player_height));

    // 自己的框也按姿态缩放（方便自己蹲下验证；菜单里勾上“给自己画框”就能看到）
    // 真头时不用再乘比例（真头已经跟着蹲下/趴下了）。
    let head_world = {
        let st = memory::read::<i32>(sp_ptr + data::ROLE_STAND_OFF).unwrap_or(1);
        if config.crouch_follow && real_head.is_none() {
            let scale = data::stance_height_scale(st);
            Vec3 {
                x: base_head.x,
                y: sp.y + config.player_height * scale,
                z: base_head.z,
            }
        } else {
            base_head
        }
    };
    let (Some(h), Some(f)) = (
        memory::world_to_screen(head_world, matrix, screen_w, screen_h, proj),
        memory::world_to_screen(sp, matrix, screen_w, screen_h, proj),
    ) else {
        ui.get_foreground_draw_list().add_text(
            [20.0, screen_h * 0.5],
            [1.0, 0.0, 1.0, 1.0],
            "SELF: 投影失败(在相机后面)",
        );
        return;
    };

    let c = [1.0, 0.0, 1.0, 1.0];
    let hh = (f.y - h.y).abs().max(6.0);
    let ww = hh * 0.45;
    let dl = ui.get_foreground_draw_list();
    dl.add_rect(
        [h.x - ww * 0.5, h.y.min(f.y)],
        [h.x + ww * 0.5, h.y.max(f.y)],
        c,
    )
    .thickness(2.0)
    .build();
    dl.add_text([h.x + 6.0, h.y - 30.0], c, "SELF");
    dl.add_text(
        [h.x + 6.0, h.y - 16.0],
        c,
        &format!("({:.0},{:.0},{:.0})", sp.x, sp.y, sp.z),
    );
}
