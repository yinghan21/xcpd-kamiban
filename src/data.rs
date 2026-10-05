use crate::log;
use crate::memory::{self, Vec3};
use crate::offset;
// 偏移常量统一住在 offset.rs。这里 re-export 一份：
// 既让本文件里 `offset::X` 的老写法继续可用，也让 `crate::data::ROLE_POS_OFF` 这类外部引用不受影响。
pub use crate::offset::*;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// 坐标在内存里的布局（由菜单实时切换）：
/// 0 = 8 字节间隔（原始读法: +0=x, +8=z, +16=y）
/// 1 = 连续 4 字节 (x, y, z)
/// 2 = 连续 4 字节 (x, z, y)
pub static LAYOUT: AtomicUsize = AtomicUsize::new(0);


/// 实时高度微调（米）：实时位置是角色根节点，比脚底略高，这里减去一个常量。
/// 由菜单滑块实时设置（存 f32 位模式）。
pub static LIVE_Y_ADJUST: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// 当前“实时高度微调”值（米）。
pub fn live_y_adjust() -> f32 {
    f32::from_bits(LIVE_Y_ADJUST.load(Ordering::Relaxed))
}

/// 位置是否像“真实的地图坐标”（过滤 (0,0,0)/原点附近/离谱值这类野框）。
fn plausible(p: Vec3) -> bool {
    p.x.is_finite()
        && p.y.is_finite()
        && p.z.is_finite()
        && (p.x.abs() > 50.0 || p.z.abs() > 50.0)
        && p.x.abs() < 30000.0
        && p.z.abs() < 30000.0
        && p.y > -200.0
        && p.y < 8000.0
}

/// 画框用的位置：
/// - x/z 用实时位置（跳/空中都准）
/// - y 做去抖动：角色根节点会被走路动画带着上下抖，只有明显高于地面
///   （>0.5m）时才认为真的在跳，否则用平滑的地面高度。
/// - self_pos 给定时，远处实体不读实时位置（省开销）。
pub fn read_draw_pos(ptr: usize, self_pos: Option<Vec3>) -> Option<Vec3> {
    let g = read_vec3_fast(ptr + crate::offset::ENTITY_POS_OFFSET);
    let g_ok = matches!(g, Some(v) if plausible(v));

    // 远处的实体不读实时位置（省开销；远处也看不出跳跃）。
    // 注意：地面坐标无效(空中/未出生)时必须读实时位置——否则永远画不出来。
    let near = match (g, self_pos) {
        (Some(gg), Some(sp)) if g_ok => {
            let dx = gg.x - sp.x;
            let dz = gg.z - sp.z;
            dx * dx + dz * dz < 250.0 * 250.0
        }
        _ => true,
    };
    let l = if near { read_live_pos(ptr) } else { None };
    let l_ok = matches!(l, Some(v) if plausible(v));

    match (g, l) {
        (Some(g), Some(l)) if g_ok && l_ok => {
            if (l.x - g.x).abs() < 60.0 && (l.z - g.z).abs() < 60.0 {
                let ly = l.y - live_y_adjust();
                let y = if ly > g.y + 0.5 { ly } else { g.y };
                Some(Vec3 { x: l.x, y, z: l.z })
            } else {
                Some(g)
            }
        }
        (_, Some(l)) if l_ok => Some(Vec3 {
            x: l.x,
            y: l.y - live_y_adjust(),
            z: l.z,
        }),
        (Some(g), _) if g_ok => Some(g),
        _ => None,
    }
}

/// 读某个实体的实时位置（跳/空中会跟随）：实体 +0x1E8 → 位置对象 → +0xA0。
pub fn read_live_pos(ptr: usize) -> Option<Vec3> {
    let po = memory::read::<usize>(ptr + LIVE_POS_PTR)?;
    if po < 0x10000 || (po >> 44) != 0 {
        return None;
    }
    let x = memory::read::<f32>(po + LIVE_POS_OFF)?;
    let y = memory::read::<f32>(po + LIVE_POS_OFF + 4)?;
    let z = memory::read::<f32>(po + LIVE_POS_OFF + 8)?;
    if x.is_finite() && y.is_finite() && z.is_finite() {
        Some(Vec3 { x, y, z })
    } else {
        None
    }
}

fn rf(addr: usize) -> Option<f32> {
    memory::read::<f32>(addr)
}

// ========================= 头顶坐标（动态跟随蹲下） =========================
//
// 实体自身的 +0x15C 附近只有“落地位置”，没有头顶。头顶藏在一个子对象里：
//   实体 +0x110 → 子对象 →(可能还有一层)→ 某个三元组 = 头顶
// 具体是第几层、偏移多少，扫描时才知道，所以这里做“首次自动定位 + 缓存”：
//   1) 第一次读时，在【实时位置对象 / 实体+0x110 的子对象 / 它的孙对象】里找
//      “水平贴着脚、比脚高 0.7~2.4 米”的三元组，取最高的那个当头顶，记下路径；
//   2) 之后每帧只读一次指针 + 一次三元组，开销可忽略。
//
// 路径编码：
//   HEAD_CHILD_OFF = 实体里第一层对象指针的偏移
//   HEAD_MID_OFF   = 第一层对象里第二层指针的偏移（HEAD_DIRECT = 头就在第一层里）
//   HEAD_FIELD_OFF = 目标对象里三元组的偏移
static MATRIX_ADDR_FAILS: AtomicUsize = AtomicUsize::new(0);

/// CE 里扫出来的静态链（对本版本）：
/// UnityPlayer.dll + 0x1C29C10 → +180 → +40 → +100 → +18 → +280
/// 解析出的对象里，**玩家对象就在 +0x700 处**（即 列表基址 + 224*8），
/// 正好对应旧代码 “list + i*8” 的读法。
#[allow(dead_code)]
fn player_list_base_ce(module: usize) -> Option<usize> {
    if module < 0x10000 {
        return None;
    }
    let a = memory::read::<usize>(module + 0x1C29C10)?;
    let b = memory::read::<usize>(a + 0x180)?;
    let c = memory::read::<usize>(b + 0x40)?;
    let d = memory::read::<usize>(c + 0x100)?;
    let e = memory::read::<usize>(d + 0x18)?;
    memory::read::<usize>(e + 0x280)
}

// ===================== 手动触发矩阵重定位 =====================

// ===================== 自动找玩家数组 =====================
/// 注入后自动跑一次“全内存找对象数组”并写日志 —— 省得在长菜单里找按钮。
/// 默认关：这个扫描要逐个指针读内存，太慢且会拖帧，需要时手动开。

// ===================== 按类名枚举角色（BattleRoleLogic） =====================
// 角色 / RoleNet / RoleNetMirror / RoleAILogic 的偏移全在 offset.rs（本文件顶部已 re-export）。
// 那边的注释里带了每条偏移的实测来源，改偏移时只改 offset.rs。

/// 是否用“按类名枚举角色”代替那条坏链（默认开）
pub static USE_ROLE_SCAN: AtomicBool = AtomicBool::new(true);

static ROLE_OBJS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
/// 最近一次的“自己”角色对象指针（read_into 更新），供血量基线/对比用。
static LAST_SELF_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 过滤一组候选角色对象：统计类名分布 + 坐标合法性；类名恰好为 `BattleRoleLogic` 的优先采用。
/// 返回 (采用列表, 是否类名匹配, 类名分布字符串)。
fn filter_roles(raw: &[usize]) -> (Vec<usize>, bool, String) {
    let mut names: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut v: Vec<usize> = Vec::new();
    let mut v_match: Vec<usize> = Vec::new();
    for &o in raw {
        let name = match memory::read::<usize>(o) {
            Some(k) => memory::il2cpp_class_get_name_api(k).unwrap_or_else(|| "?".into()),
            None => "?".into(),
        };
        *names.entry(name.clone()).or_insert(0) += 1;
        let Some(p) = read_role_pos(o + ROLE_POS_OFF) else {
            continue;
        };
        if !(p.x.is_finite() && p.y.is_finite() && p.z.is_finite()) {
            continue;
        }
        // 地图坐标范围放宽到 ±20000：出生岛在 ±5000 附近，以前 5000 的阀值会把
        // 出生岛的自己当作“无效坐标”过滤掉（日志里 采用=0 就是这个原因）。
        if p.x.abs() > 20000.0 || p.z.abs() > 20000.0 {
            continue;
        }
        if name == ROLE_CLASS_NAME {
            v_match.push(o);
        }
        v.push(o);
    }
    let matched = !v_match.is_empty();
    let v = if matched { v_match } else { v };
    let mut dist: Vec<(String, usize)> = names.into_iter().collect();
    dist.sort_by(|a, b| b.1.cmp(&a.1));
    let dist_s: Vec<String> = dist
        .iter()
        .take(8)
        .map(|(k, c)| format!("{k}×{c}"))
        .collect();
    (v, matched, dist_s.join(", "))
}

/// 打印角色列表前 n 个：地址 / 坐标 / 队伍 / 姿态 / 昵称指针。
fn dump_roles(v: &[usize], n: usize) -> String {
    let mut s = format!("  --- 角色(前{n}) ---\n");
    for o in v.iter().take(n) {
        let ps = read_role_pos(*o + ROLE_POS_OFF)
            .map(|x| format!("({:.1},{:.1},{:.1})", x.x, x.y, x.z))
            .unwrap_or_else(|| "?".into());
        let team = memory::read::<i64>(*o + 0x7B0).unwrap_or(-1);
        let st = memory::read::<f32>(*o + ROLE_STAND_OFF).unwrap_or(-1.0);
        let nm = memory::read::<usize>(*o + ROLE_NAME_OFF).unwrap_or(0);
        s.push_str(&format!(
            "    0x{o:012X} pos={ps} team={team} stand={st:.2} nameStr=0x{nm:X}\n"
        ));
    }
    s
}

/// 把角色列表存进全局。
fn store_roles(v: Vec<usize>) {
    if let Ok(mut g) = ROLE_OBJS.lock() {
        *g = v;
    }
}

/// 人机候选：StartGame.MyRoleAIMgr → RoleAIManager.logicList。
/// 返回 (BattleRoleLogic指针 或 0, RoleAILogic指针)。
/// 人机在很多模式里不在 StartGame.RoleList，但一定在 AI 管理器里。
fn ai_candidates() -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for a in memory::ai_logic_list(512) {
        if a < 0x10000 {
            continue;
        }
        // RoleAILogic.gourdRoleLogic @0x1F0 指回它的 BattleRoleLogic
        let role = memory::read::<usize>(a + 0x1F0).unwrap_or(0);
        if role >= 0x10000 && looks_like_role(role) {
            out.push((role, a));
        } else {
            out.push((0, a));
        }
    }
    out
}

/// 粗校验：这个指针看起来是不是一个“有合法坐标的角色”（用于合并备选链/补扫结果）。
fn looks_like_role(e: usize) -> bool {
    e >= 0x10000
        && matches!(
            memory::read_vec3_contig(e + ROLE_POS_OFF),
            Some(p) if p.x.is_finite() && p.y.is_finite() && p.z.is_finite()
                && p.x.abs() < 20000.0 && p.z.abs() < 20000.0
        )
}

/// 扫一次角色对象，结果存进全局。返回扫到的个数。
/// ① 权威静态链 `GameWorldClientManager → BattleWorld.startGame → StartGame.RoleList`；
/// ② 备选静态链 `AuxiliaryAimingUtil.roleList`（官方 API 或手工 static_fields）；
/// ③ 兜底：全内存扫“值 == 类指针”（会混入元数据表项，仅作回退）。
pub fn scan_roles() -> usize {
    // ① 权威静态链
    let (raw, wdbg) = memory::battle_roles_verbose();
    if !raw.is_empty() {
        let (v, matched, dist) = filter_roles(&raw);
        // 抑制刷屏：列表个数没变就不重复打，每 5 秒最多一条。
        // （之前 400ms 打一遍 → 一分钟上千行，日志没法看）
        let now = now_ms();
        let changed = v.len() != SCAN_LAST_N.load(Ordering::Relaxed);
        let due = now.saturating_sub(SCAN_LAST_LOG.load(Ordering::Relaxed)) >= 5000;
        if changed || due {
            SCAN_LAST_LOG.store(now, Ordering::Relaxed);
            SCAN_LAST_N.store(v.len(), Ordering::Relaxed);
            log::append(&format!(
                "角色扫描(权威链 StartGame.RoleList):\n{wdbg}  原始={} 采用={} (类名匹配={matched}) 类名分布: {dist}\n",
                raw.len(),
                v.len()
            ));
            if !v.is_empty() && changed {
                log::append(&dump_roles(&v, 8));
            } else if v.len() <= 1 {
                log::append("  ⚠ 列表里只有自己 → 可能还在飞机/加载中（这阶段游戏本地没建别人的角色对象）\n");
            }
        }
        if !v.is_empty() {
            let mut v = v;
            if v.len() <= 1 {
                // 出生岛/飞机：权威链只有自己 → 补备选链 AuxiliaryAimingUtil.roleList
                if let Some(extra) = memory::roles_from_static_list(512) {
                    let mut added = 0usize;
                    for e in extra {
                        if looks_like_role(e) && !v.contains(&e) {
                            v.push(e);
                            added += 1;
                        }
                    }
                    // 只在真的补到东西时打日志（否则每 400ms 一行，刷屏），且最多 10 秒一条
                    if added > 0
                        && now_ms().saturating_sub(AUX_LOG_MS.load(Ordering::Relaxed)) >= 10_000
                    {
                        AUX_LOG_MS.store(now_ms(), Ordering::Relaxed);
                        log::append(&format!(
                            "  备选链 AuxiliaryAimingUtil.roleList 补入 {added} 个\n"
                        ));
                    }
                }
            }
            let n = v.len();
            store_roles(v);
            return n;
        }
        log::append("  （权威链拿到列表但过滤后为空 → 试备选链）\n");
    } else {
        log::append(&format!("角色扫描(权威链 StartGame.RoleList)失败:\n{wdbg}"));
    }

    // ② 备选静态链：AuxiliaryAimingUtil.roleList
    let (list_opt, dbg) = memory::aux_role_list_verbose();
    if let Some(list) = list_opt {
        if let Some((items, size)) = memory::read_list(list) {
            let objs = memory::read_array_items(items, size, 512);
            let (v, matched, dist) = filter_roles(&objs);
            log::append(&format!(
                "角色扫描(备选链 AuxiliaryAimingUtil.roleList):\n{dbg}  List=0x{list:012X} size={size} 元素={} 采用={} (类名匹配={matched}) 类名分布: {dist}\n",
                objs.len(),
                v.len()
            ));
            if !v.is_empty() {
                log::append(&dump_roles(&v, 8));
                let n = v.len();
                store_roles(v);
                return n;
            }
            log::append("  （备选链拿到 List，但过滤后为空 → 回退扫值法）\n");
        } else {
            log::append(&format!(
                "角色扫描(备选链):\n{dbg}  read_list(List) 失败 → 回退扫值法\n"
            ));
        }
    } else {
        log::append(&format!("角色扫描(备选链)失败 → 回退扫值法:\n{dbg}"));
    }

    // ===== 兜底：全内存扫描（非常重，秒级）=====
    // ⚠️ 之前这里是“无条件执行”：只要权威链+备选链都拿不到角色，就每 400ms 全内存扫一遍
    //（实测每次 ~950ms）。出生岛上就会变成“扫描线程永不停歇 → CPU 打满 → 卡顿”。
    // 现在改成：默认关，需手动开；即使开了也最多 10 秒跑一次。
    if !HEAVY_SCAN.load(Ordering::Relaxed) {
        return 0;
    }
    let now_h = now_ms();
    if now_h.saturating_sub(HEAVY_SCAN_LAST.load(Ordering::Relaxed)) < HEAVY_SCAN_INTERVAL_MS {
        return 0;
    }
    HEAVY_SCAN_LAST.store(now_h, Ordering::Relaxed);

    // 兜底 2：结构校验的全内存扫描（穿透任何容器，直接找真对象）
    let structural = memory::scan_roles_by_structure(80000);
    if structural.len() > 1 {
        let (v, matched, dist) = filter_roles(&structural);
        log::append(&format!(
            "角色扫描(结构扫描): 真对象={} 采用={} (类名匹配={matched}) 类名分布: {dist}\n",
            structural.len(),
            v.len()
        ));
        if !v.is_empty() {
            log::append(&dump_roles(&v, 8));
            let n = v.len();
            store_roles(v);
            return n;
        }
    }

    // ===== 兜底：全内存扫“值 == 类指针” =====
    // 用 IL2CPP 官方 API 按名字拿类指针（不依赖名字偏移）
    let Some(klass) = memory::il2cpp_find_class("", ROLE_CLASS_NAME) else {
        log::append(&format!(
            "角色扫描: 没找到类 {ROLE_CLASS_NAME}（il2cpp API 不可用或名字不对）\n"
        ));
        return 0;
    };
    let cname = memory::il2cpp_class_get_name_api(klass).unwrap_or_else(|| "?".to_string());
    let raw = memory::scan_objects_by_class(klass, 40000);
    // 只保留“+0xA4 处是一个像地图坐标的 Vector3”的（过滤垃圾）
    let mut v: Vec<usize> = Vec::new();
    let (mut c_head, mut c_pos, mut c_name) = (0usize, 0usize, 0usize);
    for o in raw.iter() {
        // ① 对象头必须是该类
        if memory::read::<usize>(*o) != Some(klass) {
            continue;
        }
        c_head += 1;
        // ② 坐标必须在地图范围内（收紧：|x|,|z|<3000, 0<=y<=500）
        let Some(p) = read_role_pos(*o + ROLE_POS_OFF) else {
            continue;
        };
        if !(p.x.is_finite() && p.y.is_finite() && p.z.is_finite())
            || p.x.abs() > 5000.0
            || p.z.abs() > 5000.0
            || !(-500.0..=2000.0).contains(&p.y)
            || (p.x.abs() < 1.0 && p.z.abs() < 1.0)
        {
            continue;
        }
        c_pos += 1;
        // ③ 昵称合法（强判别，但只做统计，不做硬门槛）
        let mut name_ok = false;
        if let Some(np) = memory::read::<usize>(*o + ROLE_NAME_OFF) {
            if np > 0x10000 {
                if let Some(len) = memory::read::<i32>(np + 0x10) {
                    if (1..=24).contains(&len) {
                        name_ok = true;
                    }
                }
            }
        }
        if name_ok {
            c_name += 1;
        }
        v.push(*o);
        if v.len() >= 150 {
            break;
        }
    }
    let n = v.len();
    log::append(&format!(
        "角色扫描: 类名={cname} 类=0x{klass:012X}  命中 {} → 头校验 {c_head} → 坐标校验 {c_pos} → 昵称合法 {c_name} → 采用 {n}\n",
        raw.len()
    ));
    // 调试：打印前 10 个候选的字段，用来分辨真假
    if !v.is_empty() {
        let mut dbg = String::from("  --- 候选对象(前10) ---\n");
        for o in v.iter().take(10) {
            let p = read_role_pos(*o + ROLE_POS_OFF);
            let st = memory::read::<i32>(*o + ROLE_STAND_OFF);
            let r718 = memory::read::<usize>(*o + 0x718).unwrap_or(0);
            let r770 = memory::read::<usize>(*o + ROLE_NAME_OFF).unwrap_or(0);
            let ps = p
                .map(|x| format!("({:.1},{:.1},{:.1})", x.x, x.y, x.z))
                .unwrap_or_else(|| "?".to_string());
            dbg.push_str(&format!(
                "    0x{o:012X} pos@0xA4={ps} stand@0x218={st:?} +718=0x{r718:X} +770=0x{r770:X}\n"
            ));
        }
        log::append(&dbg);
    }
    // ⚠️ 扫值法匹配到的多是元数据表项（会画成“野指针框”），绝不写入 ROLE_OBJS。
    log::append("  （扫值法结果不可信 → 丢弃，保留上一次的角色列表）\n");
    0
}

// (旧的单次血量对比已被三快照判定取代)

// 角色链每一步都校验类名（否则旧链垃圾实体也会被当成“角色”，读出假血量）
static KLASS_ROLENET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static KLASS_ROLEMIRROR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static KLASS_ROLEAILOGIC: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn cached_klass(name: &str, slot: &std::sync::atomic::AtomicUsize) -> usize {
    let k = slot.load(Ordering::Relaxed);
    if k != 0 {
        return k;
    }
    let k = memory::il2cpp_find_class("", name).unwrap_or(0);
    if k != 0 {
        slot.store(k, Ordering::Relaxed);
    }
    k
}

/// 该地址的首字段是不是这个类（klass==0 表示未知 → 跳过校验，宽松处理）
fn obj_is_klass(p: usize, klass: usize) -> bool {
    klass == 0 || memory::read::<usize>(p) == Some(klass)
}

/// 角色 → RoleNet（+0x740，类名校验）
fn role_net(role: usize) -> Option<usize> {
    let n = memory::read::<usize>(role + ROLE_NET_OFF)?;
    if n < 0x10000 {
        return None;
    }
    if !obj_is_klass(n, cached_klass("RoleNet", &KLASS_ROLENET)) {
        return None;
    }
    Some(n)
}
/// 角色 → RoleNetMirror（RoleNet.mirror @0x90，类名校验）
fn role_mirror(role: usize) -> Option<usize> {
    let m = memory::read::<usize>(role_net(role)? + ROLE_MIRROR_OFF)?;
    if m < 0x10000 {
        return None;
    }
    if !obj_is_klass(m, cached_klass("RoleNetMirror", &KLASS_ROLEMIRROR)) {
        return None;
    }
    Some(m)
}
/// 角色 → RoleAILogic（RoleNet.$c @0x80，类名校验）；非空 = 人机
fn role_ailogic(role: usize) -> Option<usize> {
    let a = memory::read::<usize>(role_net(role)? + ROLE_AILOGIC_OFF)?;
    if a < 0x10000 {
        return None;
    }
    if !obj_is_klass(a, cached_klass("RoleAILogic", &KLASS_ROLEAILOGIC)) {
        return None;
    }
    Some(a)
}
/// 是否本地玩家：RoleNet.roleNetClient(+0x58).isLocalPlayer(+0x1D)
fn is_local_role(role: usize) -> bool {
    let ok = (|| -> Option<bool> {
        let c = memory::read::<usize>(role_net(role)? + ROLE_NETCLIENT_OFF)?;
        if c < 0x10000 {
            return None;
        }
        Some(memory::read::<u8>(c + 0x1D)? != 0)
    })();
    ok.unwrap_or(false)
}
/// 读角色血量：真人走 RoleNetMirror.SyncHp（权威）；人机走 RoleAILogic.Hp。
/// ⚠️ 人机的 Hp 常常恒为 0（服务端不下发该属性）——以前把它当成“血量为 0 → 已死”，
/// 结果 ESP 里人机全被当尸体跳过了（就是“人机不绘制”的真正原因）。现在 0 当“未知”。
fn read_role_hp(role: usize) -> Option<f32> {
    if let Some(m) = role_mirror(role) {
        if let Some(hp) = memory::read::<f32>(m + ROLE_HP_OFF) {
            if hp.is_finite() && (-1.0..=1000.0).contains(&hp) {
                return Some(hp);
            }
        }
    }
    if let Some(a) = role_ailogic(role) {
        if let Some(hp) = memory::read::<f32>(a + AILOGIC_HP_OFF) {
            // 只接受 > 0 的值；0/负数 = “没下发”，返回 None 让上层当作“未知”
            if hp.is_finite() && hp > 0.0 && hp <= 1000.0 {
                return Some(hp);
            }
        }
    }
    None
}
/// 读角色昵称：0x770 → mirror.SyncNickName → RoleAILogic.Name → 0x278
fn read_role_name(role: usize) -> Option<String> {
    if let Some(s) = memory::read_il2cpp_string(role + ROLE_NAME_OFF) {
        if !s.is_empty() {
            return Some(s);
        }
    }
    if let Some(m) = role_mirror(role) {
        if let Some(s) = memory::read_il2cpp_string(m + MIRROR_NICK_OFF) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    if let Some(a) = role_ailogic(role) {
        if let Some(s) = memory::read_il2cpp_string(a + AILOGIC_NAME_OFF) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    memory::read_il2cpp_string(role + ROLE_NAME2_OFF).filter(|s| !s.is_empty())
}
/// 读角色队伍号：0x7B0 → mirror.SyncTeamNum → RoleAILogic.TeamId(字符串)
fn read_role_team(role: usize) -> i64 {
    if let Some(v) = memory::read::<i64>(role + ROLE_TEAM_OFF) {
        return v;
    }
    if let Some(m) = role_mirror(role) {
        if let Some(v) = memory::read::<i64>(m + MIRROR_TEAM_OFF) {
            return v;
        }
    }
    if let Some(a) = role_ailogic(role) {
        if let Some(s) = memory::read_il2cpp_string(a + AILOGIC_TEAM_OFF) {
            if let Ok(v) = s.trim().parse::<i64>() {
                return v;
            }
        }
    }
    -1
}

/// 读角色真实姿态枚举：RoleNetMirror.SyncStandbyState @0x194。
#[allow(dead_code)]
fn read_role_stance(role: usize) -> Option<i32> {
    let net = memory::read::<usize>(role + ROLE_NET_OFF)?;
    if net < 0x10000 {
        return None;
    }
    let mirror = memory::read::<usize>(net + ROLE_MIRROR_OFF)?;
    if mirror < 0x10000 {
        return None;
    }
    memory::read::<i32>(mirror + ROLE_STANCE_OFF)
}

/// 姿态枚举 → 身高比例（“没有真头”时用它估框顶高度，让框跟着蹲下/趴下变矮）。
/// AC_StandState: Stand=1 / Crouch=2 / Prone=3 / Fly=4 …
pub fn stance_height_scale(stance_enum: i32) -> f32 {
    match stance_enum {
        2 => 0.65, // 蹲下
        3 => 0.30, // 趴下
        _ => 1.0,
    }
}

// ===================== 头部骨骼/坐标定位（客户端链 + 蹲下实证） =====================
// ESP/自瞄用的是 BattleRoleLogic（逻辑层）。真正“会跟着头动”的世界坐标在客户端层：
//   BattleRoleLogic +0xAF0 → RoleLogicClient
//   RoleLogicClient +0x80  → BattleRole（RoleClient）
//   BattleRole      +0x240 → RoleControl（MonoBehaviour，模型控制）
//   RoleControl     +0x16C → Vector3 headPoint（游戏自己的 GetHeadPoint()）
//   RoleControl（基类 RoleControlBase）+0x28 身高 / +0x20 中心Y / +0x2C 半径 / +0x30 _myTransform
//   BattleRole      +0x20  → List<BattleComponent>（里面有 BattleRoleCameraComponent._headPoint @0x58）
// 客户端模型链（RoleLogicClient → BattleRole → RoleControl → headPoint）的偏移全在 offset.rs。
// 链路说明见那边第 ⑥ 组，改偏移只改 offset.rs。

/// 是否用“真头（客户端链 headPoint）”（默认开）。关掉就一律走“脚 + 身高”。
pub static USE_REAL_HEAD: AtomicBool = AtomicBool::new(true);

/// 隐藏离线/退出玩家（默认开）。
pub static HIDE_OFFLINE: AtomicBool = AtomicBool::new(true);

/// 读侧身（镜像同步值）+ 朝向 yaw。
pub fn read_side_yaw(role: usize) -> (i32, f32, i32) {
    let mirror = (|| -> Option<usize> {
        let net = memory::read::<usize>(role + ROLE_NET_OFF)?;
        if net < 0x10000 {
            return None;
        }
        let m = memory::read::<usize>(net + ROLE_MIRROR_OFF)?;
        if m < 0x10000 {
            return None;
        }
        Some(m)
    })();
    let side = mirror
        .and_then(|m| memory::read::<i32>(m + MIRROR_SIDEAIM_OFF))
        .unwrap_or(0);
    let stance = mirror
        .and_then(|m| memory::read::<i32>(m + MIRROR_STANCE_OFF))
        .unwrap_or(-1);
    let yaw = (|| -> Option<f32> {
        let q = memory::read::<[f32; 4]>(role + 0x7E8)?;
        let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
        let n = (x * x + y * y + z * z + w * w).sqrt();
        if !n.is_finite() || n < 1e-3 {
            return None;
        }
        Some((2.0 * (w * y + x * z)).atan2(1.0 - 2.0 * (y * y + z * z)))
    })()
    .unwrap_or(0.0);
    (side, yaw, stance)
}

/// 探头量的平滑器开关（跟模型探头动画同步，避免挠点跑到头前面）。
pub static LEAN_ON: AtomicBool = AtomicBool::new(true);

/// 探头量的平滑器（按 ptr 记状态；一阶低通，时间常数 0.18s，跟模型动画近似同步）。
static LEAN_SM: std::sync::Mutex<Vec<(usize, f32, u64)>> = std::sync::Mutex::new(Vec::new());

fn smooth_lean(ptr: usize, side: i32) -> f32 {
    let target = if side > 0 {
        1.0
    } else if side < 0 {
        -1.0
    } else {
        0.0
    };
    let now = now_ms();
    let mut g = match LEAN_SM.lock() {
        Ok(g) => g,
        Err(_) => return target,
    };
    if let Some(e) = g.iter_mut().find(|e| e.0 == ptr) {
        let dt = ((now.saturating_sub(e.2)) as f32 / 1000.0).clamp(0.001, 0.2);
        let a = 1.0 - (-dt / 0.18).exp();
        e.1 += (target - e.1) * a;
        e.2 = now;
        e.1.clamp(-1.0, 1.0)
    } else {
        if g.len() > 256 {
            g.remove(0);
        }
        g.push((ptr, target, now));
        target
    }
}

/// 头部水平补偿（世界坐标 XZ 增量）。
/// Unity: forward=(sin yaw, 0, cos yaw)，right=(cos yaw, 0, -sin yaw)。
/// - 趴下：头在身体**前方**约 `prone_fwd_m`（headPoint 只给高度、不给前移→旧版趴下只能瞄到背）
/// - 蹲下：头往前一点
/// - 探头/侧身：头往右/左平移（`SyncSideAim` = ±1）
pub fn head_shift(config: &crate::config::Config, p: &Player) -> (f32, f32) {
    // 滑翔/飞行(Fly=4) 中不做任何头部水平补偿：此时 headPoint 在滑翔翼上，
    // 补偿会把框/瞄点推到身体前面去（用户反馈“滑翔时框跑到敌人特别前面”）。
    if p.stance_enum == 4 {
        return (0.0, 0.0);
    }
    let (sy, cy) = p.yaw.sin_cos();
    let fwd = match p.stance_enum {
        3 => config.prone_fwd_m,
        2 => config.crouch_fwd_m,
        _ => 0.0,
    };
    let mut lat = 0.0;
    if config.lean_follow {
        let mut s = p.lean_smooth.clamp(-1.0, 1.0);
        if config.lean_invert {
            s = -s;
        }
        lat = config.lean_shift_m * s;
    }
    (sy * fwd + cy * lat, cy * fwd - sy * lat)
}

/// 兼容旧调用名。
pub fn lean_shift(config: &crate::config::Config, p: &Player) -> (f32, f32) {
    head_shift(config, p)
}

/// 客户端链每一跳的类指针缓存（3 跳的类对象对所有角色都一样，解析一次即可；
/// 每帧对每个角色只做 `对象[0] == klass` 的整数比较，不再调类名 API）。
static KLASS_RLC: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static KLASS_BATTLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static KLASS_RCTRL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 读“真头”世界坐标（实测 2026-10-04 已验证）：
///   BattleRoleLogic +0xAF0 → RoleLogicClient → +0x80 BattleRole → +0x240 RoleControl → +0x16C headPoint
/// 4 个角色 dY 全是 1.26~1.27、水平差 0.00；自己蹲下 y 从 80.49 掉到 79.98。
/// 校验：逐跳类名 + 头必须比脚高 0.4~2.3m 且水平贴脚 2.5m 内；不满足返回 None（回退“脚+身高”）。
pub fn read_role_head(role: usize) -> Option<Vec3> {
    let feet = read_role_pos(role + ROLE_POS_OFF)?;

    let rlc = memory::read::<usize>(role + ROLE_LOGICCLIENT_OFF)?;
    if rlc < 0x10000 || !obj_is_klass(rlc, cached_klass("RoleLogicClient", &KLASS_RLC)) {
        return None;
    }
    let br = memory::read::<usize>(rlc + RLC_BATTLEROLE_OFF)?;
    if br < 0x10000 || !obj_is_klass(br, cached_klass("BattleRole", &KLASS_BATTLE)) {
        return None;
    }
    let ctrl = memory::read::<usize>(br + BATTLE_ROLECONTROL_OFF)?;
    if ctrl < 0x10000 || !obj_is_klass(ctrl, cached_klass("RoleControl", &KLASS_RCTRL)) {
        return None;
    }
    let h = memory::read_vec3_contig(ctrl + ROLECONTROL_HEAD_OFF)?;
    let dy = h.y - feet.y;
    if !(0.40..=2.30).contains(&dy) {
        return None;
    }
    // 水平必须几乎贴脚（实测正常 0.00~0.2m）。滑翔时 headPoint 可能挂在滑翔翼上、
    // 水平偏出好几米 → 这里收紧到 1.0m，超了就回退“脚+身高”，免得框跑到身体前面。
    if (h.x - feet.x).abs() > 1.0 || (h.z - feet.z).abs() > 1.0 {
        return None;
    }
    Some(h)
}




/// 是否在线：`BattleRoleLogic.isOnline @0xB24`。
/// 退出/离线的角色对象还留在 StartGame.RoleList 里且位置冻结 → 幽灵框。
/// 读不到就当作在线（宁可不拦）。
pub fn role_is_online(role: usize) -> bool {
    match memory::read::<u8>(role + ROLE_ONLINE_OFF) {
        Some(v) => v != 0,
        None => true,
    }
}

/// 幽灵过滤日志（每个指针只打一次，攒多了清空）。
static OFFLINE_LOGGED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
fn log_offline(role: usize, name: &str) {
    if let Ok(mut v) = OFFLINE_LOGGED.lock() {
        if v.contains(&role) {
            return;
        }
        if v.len() > 256 {
            v.clear();
        }
        v.push(role);
    }
    let nm = if name.is_empty() { "?" } else { name };
    log::append(&format!(
        "[幽灵过滤] 隐藏离线玩家 {nm} (0x{role:012X}, isOnline=0)\n"
    ));
}

// ===================== 幽灵过滤：位置冻结检测 =====================
// 退出/传送走的玩家，角色对象常常仍留在 StartGame.RoleList 里，位置再也不更新。
// （实测日志：170 个扫描周期里好几个指针位置一直不变。）
// isOnline 对这些对象不一定是 0，所以再加一道“位置长时间没动”的判定。
pub static GHOST_FILTER: AtomicBool = AtomicBool::new(true);
pub static GHOST_FREEZE_MS: AtomicU64 = AtomicU64::new(3000);

struct GhostPos {
    pos: [f32; 3],
    yaw: f32,
    move_ms: u64,
    seen_ms: u64,
}
static GHOST_POS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, GhostPos>>,
> = std::sync::OnceLock::new();

/// 位置/朝向冻结判定：返回 true = 该角色长时间完全不动（幽灵）。
/// 注意：只有“位置也没动 且 朝向也没变”才算冻结。
/// 真人就算站着不动，只要转头（NowRotation 变）就会被当成活人。
fn ghost_check(role: usize, pos: Vec3, yaw: f32, now: u64) -> bool {
    let m = GHOST_POS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut g = match m.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let p = [pos.x, pos.y, pos.z];
    if let Some(e) = g.get_mut(&role) {
        let d = e.pos;
        let moved = ((p[0] - d[0]).powi(2) + (p[1] - d[1]).powi(2) + (p[2] - d[2]).powi(2))
            .sqrt()
            > 0.20;
        // 朝向变化 > 3° 也算“活着”（趴着不动但转头看人的不该被当幽灵）
        let mut dyaw = (yaw - e.yaw).abs();
        if dyaw > std::f32::consts::PI {
            dyaw = std::f32::consts::TAU - dyaw;
        }
        let turned = dyaw > 3.0f32.to_radians();
        if moved {
            e.pos = p;
        }
        if moved || turned {
            e.yaw = yaw;
            e.move_ms = now;
        }
        e.seen_ms = now;
        return now.saturating_sub(e.move_ms) >= GHOST_FREEZE_MS.load(Ordering::Relaxed);
    }
    if g.len() > 1024 {
        let cutoff = now.saturating_sub(60_000);
        g.retain(|_, e| e.seen_ms >= cutoff);
    }
    g.insert(
        role,
        GhostPos {
            pos: p,
            yaw,
            move_ms: now,
            seen_ms: now,
        },
    );
    false
}

static GHOST_LOGGED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
fn log_ghost(role: usize, name: &str) {
    if let Ok(mut v) = GHOST_LOGGED.lock() {
        if v.contains(&role) {
            return;
        }
        if v.len() > 256 {
            v.clear();
        }
        v.push(role);
    }
    let nm = if name.is_empty() { "?" } else { name };
    let secs = GHOST_FREEZE_MS.load(Ordering::Relaxed) as f32 / 1000.0;
    log::append(&format!(
        "[幽灵过滤] 隐藏位置冻结角色 {nm} (0x{role:012X}, 超过 {secs:.1}s 没动)\n"
    ));
}

// ===================== 位置平滑（治“框跟随一卡一卡”） =====================
// 人机/远距离玩家的位置是服务器低频下发的（不像真人那样本地插值），
// 直接画原始坐标就会一卡一卡。这里对“画出来的”脚/头做一次时间常数可调的一阶低通。
// 用同一份平滑坐标给 ESP 和自瞄，所以框和准心依然对齐；额外滞后由自瞄的“超前补偿”顶掉。
pub static POS_SMOOTH_ON: AtomicBool = AtomicBool::new(true);
pub static POS_SMOOTH_TAU_MS: AtomicU64 = AtomicU64::new(60);

struct SmoothEntry {
    feet: Vec3,
    raw: Vec3,
    head: Vec3,
    has_head: bool,
    vel: Vec3,
    ms: u64,
}
static SMOOTH_POS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<usize, SmoothEntry>>,
> = std::sync::OnceLock::new();

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// 对脚/头做一阶低通（治“框跟随一卡一卡”），同时用**原始位置差分**估速度（供自瞄预判）。
/// 返回 (平滑后的脚, 平滑后的头, 速度 m/s)。
fn smooth_pos(role: usize, feet: Vec3, head: Option<Vec3>, now: u64) -> (Vec3, Option<Vec3>, Vec3) {
    let m = SMOOTH_POS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut g = match m.lock() {
        Ok(g) => g,
        Err(_) => return (feet, head, Vec3::default()),
    };
    if g.len() > 512 {
        let cutoff = now.saturating_sub(10_000);
        g.retain(|_, e| e.ms >= cutoff);
    }
    let e = g.entry(role).or_insert(SmoothEntry {
        feet,
        raw: feet,
        head: head.unwrap_or(feet),
        has_head: head.is_some(),
        vel: Vec3::default(),
        ms: now,
    });
    let dt = ((now.saturating_sub(e.ms)) as f32 / 1000.0).clamp(0.001, 0.25);
    e.ms = now;

    // 速度：原始位置差分 / dt，再做 EMA（抖动量很大，必须平滑）
    let inv = 1.0 / dt;
    let nvx = (feet.x - e.raw.x) * inv;
    let nvy = (feet.y - e.raw.y) * inv;
    let nvz = (feet.z - e.raw.z) * inv;
    // 单帧位移对应的速度封顶 30 m/s，避免瞬移/读错把速度炸掉
    let cap = 30.0;
    e.vel.x = lerp(e.vel.x, nvx.clamp(-cap, cap), 0.25);
    e.vel.y = lerp(e.vel.y, nvy.clamp(-cap, cap), 0.25);
    e.vel.z = lerp(e.vel.z, nvz.clamp(-cap, cap), 0.25);
    e.raw = feet;

    let tau = POS_SMOOTH_TAU_MS.load(Ordering::Relaxed) as f32 / 1000.0;
    let on = POS_SMOOTH_ON.load(Ordering::Relaxed) && tau > 0.001;
    if on {
        let a = 1.0 - (-dt / tau).exp();
        e.feet.x = lerp(e.feet.x, feet.x, a);
        e.feet.y = lerp(e.feet.y, feet.y, a);
        e.feet.z = lerp(e.feet.z, feet.z, a);
    } else {
        e.feet = feet;
    }
    let out_head = match head {
        Some(h) => {
            if !e.has_head {
                e.head = h;
                e.has_head = true;
            } else if on {
                let a = 1.0 - (-dt / tau).exp();
                e.head.x = lerp(e.head.x, h.x, a);
                e.head.y = lerp(e.head.y, h.y, a);
                e.head.z = lerp(e.head.z, h.z, a);
            } else {
                e.head = h;
            }
            Some(e.head)
        }
        None => {
            e.has_head = false;
            None
        }
    };
    (e.feet, out_head, e.vel)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Player {
    /// 脚（地面）位置。
    pub feet: Vec3,
    /// 真实头顶坐标（读到才有的，蹲下会跟着降）。读不到时用“脚 + 身高”推算。
    pub head: Option<Vec3>,
    pub health: Option<i32>,
    pub alive: bool,
    /// 实体指针（帧间唯一的身份，列表重排后它不变）
    pub ptr: usize,
    /// 水平移速（单位/秒，帧间算的），自瞄预判用
    pub vel: Vec3,
    /// 姿态字段（实体 +0x264）：实测站立 ≈0.65、蹲下/趴下 ≈0.30。
    /// 用它按比例把框压矮（这就是“蹲下/趴下跟随”的数据来源）。
    pub stance: f32,
    /// 这一帧的位置是用了上一帧的（读到了异常值）
    pub stale: bool,
    /// 是不是 AI/机器人：菜单「真人/AI标志偏移」命中 **或** 该角色挂着 RoleAILogic（真 AI）。
    /// 两者取并集 —— 只靠手填偏移的话，默认没配置时人机全会被当真人。
    pub bot: bool,
    /// 队伍号（`BattleRoleLogic.TeamNum` @0x7B0）
    #[allow(dead_code)]
    pub team: i64,
    /// 昵称（UTF-8，截断到 24 字节，尾零填充）
    pub name: [u8; 24],
    /// 是否敌人：与自己的队伍不同则为 true；队伍未知时也为 true（保守当敌人）
    pub enemy: bool,
    /// 侧身/探头（`RoleNetMirror.SyncSideAim @0x1A0`，服务器同步过来的）。0 = 没探头。
    pub side: i32,
    /// 朝向 yaw（弧度，由 `BattleRoleLogic.NowRotation @0x7E8` 四元数算）。
    pub yaw: f32,
    /// 姿态枚举（`RoleNetMirror.SyncStandbyState @0x194`）：Stand=1/Crouch=2/Prone=3
    pub stance_enum: i32,
    /// 平滑后的探头程度（-1..1）：SyncSideAim 是 0→±1 瞬变，而模型探头动画是渐入的，
    /// 直接瞬移会让挠点跑到头前面去，所以用一个和动画同步的一阶平滑。
    pub lean_smooth: f32,
}

impl Player {
    /// 昵称（去掉尾部 0）。
    pub fn name_str(&self) -> &str {
        let end = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len());
        std::str::from_utf8(&self.name[..end]).unwrap_or("")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub view_matrix: [f32; 16],
    pub valid: bool,
    pub players: Vec<Player>,
    /// 实体链解出来的基址（0 = 没解出来），调试面板用它来看附近字段
    pub entity_base: usize,
    /// 本帧认定的“自己”实体指针（上载具/下车后列表会重排，不能写死第 0 项）
    pub self_ptr: usize,
    /// 自己的世界坐标（本帧算好的，绘制/自瞄直接用，不用每帧再读一遍内存）
    pub self_pos: Option<Vec3>,
    /// 自己的队伍号（用于敌我区分）；-1 = 未知
    pub self_team: i64,
}

/// 按当前布局读一个三分量坐标，返回 (x, y=高度, z)。
fn read_xyz(addr: usize) -> Option<(f32, f32, f32)> {
    let (x, y, z) = match LAYOUT.load(Ordering::Relaxed) {
        // 连续 (x, y, z)
        1 => (rf(addr)?, rf(addr + 4)?, rf(addr + 8)?),
        // 连续 (x, z, y)
        2 => (rf(addr)?, rf(addr + 8)?, rf(addr + 4)?),
        // 8 字节间隔（原始）
        _ => (rf(addr)?, rf(addr + 16)?, rf(addr + 8)?),
    };

    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return None;
    }

    Some((x, y, z))
}

/// 供外部（手动地址功能）使用：按当前布局读一个位置。
pub fn read_position(addr: usize) -> Option<Vec3> {
    let (x, y, z) = read_xyz(addr)?;
    Some(Vec3 { x, y, z })
}

const MAX_SLOTS: usize = 512;
/// 绘制上限：全局最多就 100 个敌人，给到 100 免得被前面的垃圾槽位把真敌人挤掉。
const MAX_PLAYERS: usize = 100;

/// 玩家列表基址（实体指针数组）：ENTITY_LIST → +0x48 → +0x0 → +0x60。
pub fn player_list_base(module: usize) -> Option<usize> {
    let e0 = memory::read::<usize>(module + offset::ENTITY_LIST)?;
    let e1 = memory::read::<usize>(e0 + offset::OFFSET_X)?;
    let e2 = memory::read::<usize>(e1 + offset::OFFSET_X_OFFSET_1)?;
    memory::read::<usize>(e2 + offset::OFFSET_X_OFFSET_2)
}

/// 快速读一个三分量坐标（一次系统调用），按当前布局解析。
fn read_vec3_fast(addr: usize) -> Option<Vec3> {
    let mut b = [0u8; 24];
    let n = memory::safe_read_upto(addr, &mut b);
    if n < 12 {
        return None;
    }
    let f = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let (x, y, z) = match LAYOUT.load(Ordering::Relaxed) {
        0 if n >= 20 => (f(0), f(16), f(8)),
        2 if n >= 12 => (f(0), f(8), f(4)),
        _ => (f(0), f(4), f(8)),
    };
    if x.is_finite() && y.is_finite() && z.is_finite() {
        Some(Vec3 { x, y, z })
    } else {
        None
    }
}

/// 读角色坐标：`BattleRoleLogic.Vector3 $t @ROLE_POS_OFF`，**连排布局** (x@0, y@4, z@8)。
/// 注意：与旧实体对象用的“8 字节间隔”布局不同，这里必须连排（实证扫描已确认）。
fn read_role_pos(addr: usize) -> Option<Vec3> {
    memory::read_vec3_contig(addr)
}

/// 候选视图矩阵是否可用：
/// - 结构上必须是 VP 矩阵（见 memory::matrix_looks_valid，identity 会被拒）；
/// - 只要能拿到“参照点”（自己/附近敌人的坐标），反推出来的相机就得落在参照点附近。
///
/// strict = true 时用完整结构校验（全局扫描必须严，防假阳性）；
/// strict = false 时只要求“全是有有限数 + 带平移”（指针链是写死的路径，本来就读不错，
/// 滑翔/滞空很久后游戏会给出 w 行模长略有偏差的合法矩阵，严校验会把真矩阵也拒掉 → 框全消失）。
/// 相机是否“像本帧的相机”：**只看水平距离**。
/// 为什么不用 y：实测从矩阵反推出来的相机 y 系统性偏低约 60 米
/// （日志：相机 y=29.8，玩家 y=89.2，同 x/z）、而 x/z 是准的；
/// 带上 y 会把完全正确的矩阵也拒掉。
fn cam_plausible(c: Vec3, refs: &[Vec3], max_d: f32) -> bool {
    refs.iter().any(|p| {
        let dx = c.x - p.x;
        let dz = c.z - p.z;
        (dx * dx + dz * dz).sqrt() < max_d
    })
}

fn matrix_ok(m: &[f32; 16], refs: &[Vec3], max_d: f32) -> bool {
    if m.iter().any(|v| !v.is_finite() || v.abs() > 1.0e7) {
        return false;
    }
    // w 行模长：正常 VP 矩阵 ≈1；放宽到 0.5~1.8
    let w = (m[3] * m[3] + m[7] * m[7] + m[11] * m[11]).sqrt();
    if !(0.5..=1.8).contains(&w) {
        return false;
    }
    // 必须带平移（能反推出相机），全零/单位阵不要
    if m[12].abs() + m[13].abs() + m[14].abs() < 1.0 {
        return false;
    }
    let Some(c) = memory::camera_position(*m) else {
        return false;
    };
    if refs.is_empty() {
        return true;
    }
    cam_plausible(c, refs, max_d)
}

/// 取视图矩阵。顺序很关键：
///   ① 严格：相机水平方向就在玩家附近（第三人称 4~8 米）—— 几乎不可能误判；
///   ② 宽松：滑翔/滞空时相机可能离自己几十米。
/// 两个档都先试「全局扫描缓存地址」，再试指针链。
fn resolve_view_matrix(module: usize, refs: &[Vec3]) -> Option<[f32; 16]> {
    let cands = memory::chain_view_matrix_candidates(module);

    for m in cands.iter() {
        if matrix_ok(m, refs, 30.0) {
            MATRIX_ADDR_FAILS.store(0, Ordering::Relaxed);
            return Some(*m);
        }
    }
    if let Some(m) = memory::cached_view_matrix() {
        if matrix_ok(&m, refs, 30.0) {
            MATRIX_ADDR_FAILS.store(0, Ordering::Relaxed);
            return Some(m);
        }
    }

    for m in cands.iter() {
        if matrix_ok(m, refs, 120.0) {
            return Some(*m);
        }
    }
    if let Some(m) = memory::cached_view_matrix() {
        if matrix_ok(&m, refs, 120.0) {
            return Some(m);
        }
        // 缓存地址也不可信：连续失败很多次才清（别一失败就清，清完会触发全内存扫描 → 卡）
        let n = MATRIX_ADDR_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
        if n > 600 {
            MATRIX_ADDR_FAILS.store(0, Ordering::Relaxed);
            memory::MATRIX_ADDR.store(0, Ordering::Relaxed);
        }
    }
    None
}

/// 矩阵刷新（**每帧调**）：
/// ① 先从已定位的地址直接读 64 字节 —— 便宜，且跟着相机实时变（这是关键！
///    之前写成“只在失效时才重解析”，结果矩阵被冻结，转视角框不跟着动）；
/// ② 读不到 / 不像好矩阵 → 才重找地址；
/// ③ 找不到就**保留上一帧的**，绝不清空。
fn refresh_matrix(snapshot: &mut Snapshot, module: usize, refs: &[Vec3]) {
    let mut m = memory::cached_view_matrix();
    if m.map(|mm| !matrix_ok(&mm, refs, 30.0)).unwrap_or(true) {
        if let Some(nm) = resolve_view_matrix(module, refs) {
            m = Some(nm);
        }
    }
    if let Some(mm) = m {
        snapshot.view_matrix = mm;
    }
    if snapshot.view_matrix.iter().all(|v| v.is_finite())
        && snapshot.view_matrix.iter().any(|v| *v != 0.0)
    {
        snapshot.valid = true;
    }
}

pub fn read_into(
    UnityPlayer_module_base: usize,
    _GameAssembly_module_base: usize,
    snapshot: &mut Snapshot,
) {
    snapshot.valid = false;
    // 上一帧的玩家（顺便把它从快照里“拿走”，用于算每个敌人的移速/预判）
    let _prev_players: Vec<Player> = std::mem::take(&mut snapshot.players);

    // 玩家列表：层3 是一个“实体指针数组”，每个元素指向一个玩家实体，
    // 实体里的位置字段在 +0x15C（战场场景实测）。
    // 先读列表，顺便拿第 0 项（自己）当“矩阵是否属于主相机”的校验依据。
    // 旧链（CE 扫出来的链，重启后会失效；当年留的“手动实体表基址”入口已经撤掉）
    let list = player_list_base(UnityPlayer_module_base);
    let self_pos = list
        .and_then(|l| {
            let p = memory::read::<usize>(l)?;
            if p < 0x10000 {
                None
            } else {
                read_vec3_fast(p + offset::ENTITY_POS_OFFSET)
            }
        })
        // 空中/跳伞时位置会读成 (0,0,0)，那不是有效坐标，不能当矩阵参照点（会把正确矩阵一票否决）
        .filter(|p| p.x.abs() > 0.5 || p.z.abs() > 0.5 || p.y.abs() > 0.5);

    // ========================= ① 权威角色链（BattleRoleLogic） =========================
    // 实测链：GameWorldClientManager.MyGameWorld → BattleWorld.startGame → StartGame 的多个列表，
    // 合并去重后得到全部 BattleRoleLogic。位置在 +0x7F8（连排 x@0,y@4,z@8）。
    if USE_ROLE_SCAN.load(Ordering::Relaxed) {
        let mut roles = memory::roles_static_once(MAX_SLOTS);
        // 备选链兜底：权威链只有自己（出生岛/飞机阶段）时，补上 AuxiliaryAimingUtil.roleList。
        // v106 重建时把这步简化掉了，导致出生岛看不到人。
        if roles.len() <= 2 {
            if let Some(extra) = memory::roles_from_static_list(MAX_SLOTS) {
                for e in extra {
                    if looks_like_role(e) && !roles.contains(&e) {
                        roles.push(e);
                    }
                }
            }
        }
        // 人机来源 2：StartGame.MyRoleAIMgr → RoleAIManager.logicList（人机不在 RoleList 时靠这条）
        let ai_cands = ai_candidates();
        let mut ai_via_role = 0usize;
        for (role, _a) in ai_cands.iter() {
            if *role >= 0x10000 && !roles.contains(role) {
                roles.push(*role);
                ai_via_role += 1;
            }
        }
        if !roles.is_empty() {
            let mut me: usize = 0;
            let mut my_pos: Option<Vec3> = None;
            let mut my_team: i64 = -1;
            for &o in roles.iter() {
                if is_local_role(o) {
                    me = o;
                    my_pos = read_role_pos(o + ROLE_POS_OFF);
                    my_team = read_role_team(o);
                    break;
                }
            }
            if my_team != -1 {
                snapshot.self_team = my_team;
            }

            let mut others = 0usize;
            for &o in roles.iter() {
                if o == me {
                    continue;
                }
                // 幽灵过滤：退出/传送走的玩家角色对象还留在 RoleList 里、位置冻结。
                if HIDE_OFFLINE.load(Ordering::Relaxed) && !role_is_online(o) {
                    log_offline(o, &read_role_name(o).unwrap_or_default());
                    continue;
                }
                let Some(feet) = read_role_pos(o + ROLE_POS_OFF) else {
                    continue;
                };
                if !(feet.x.is_finite() && feet.y.is_finite() && feet.z.is_finite()) {
                    continue;
                }
                if feet.x.abs() < 0.5 && feet.z.abs() < 0.5 {
                    continue;
                }
                // 人机（bot）永远不参与“离线/幽灵”过滤 —— 用户明确要求人机必须画出来。
                // 人机判定只认 RoleAILogic（手填标志偏移那套已经撤掉）
                let ai = role_ailogic(o).is_some();
                if !ai && HIDE_OFFLINE.load(Ordering::Relaxed) && !role_is_online(o) {
                    log_offline(o, &read_role_name(o).unwrap_or_default());
                    continue;
                }
                let (side, yaw, stance_enum) = read_side_yaw(o);
                // 幽灵过滤（默认关）：位置+朝向长期冻结的（退出/传送后仍留在 RoleList 的对象）。
                // ⚠️ 它分不清“已退出”和“只是站着不动”，所以默认不开；
                // 蹲下(2)/趴下(3) 与人机直接豁免。
                if !ai
                    && GHOST_FILTER.load(Ordering::Relaxed)
                    && stance_enum != 2
                    && stance_enum != 3
                    && ghost_check(o, feet, yaw, now_ms())
                {
                    log_ghost(o, &read_role_name(o).unwrap_or_default());
                    continue;
                }
                let lean_smooth = if LEAN_ON.load(Ordering::Relaxed) {
                    smooth_lean(o, side)
                } else if side > 0 {
                    1.0
                } else if side < 0 {
                    -1.0
                } else {
                    0.0
                };
                let team = read_role_team(o);
                let hp = read_role_hp(o);
                let mut nm = [0u8; 24];
                if let Some(s) = read_role_name(o) {
                    let b = s.as_bytes();
                    let n = b.len().min(24);
                    nm[..n].copy_from_slice(&b[..n]);
                }
                let head_raw = if USE_REAL_HEAD.load(Ordering::Relaxed) {
                    read_role_head(o)
                } else {
                    None
                };
                // 位置平滑：治人机/低频同步玩家“框跟随一卡一卡”
                // （ai 在上面已经算过一次，这里不再重复读内存）
                let (feet, head, vel) = smooth_pos(o, feet, head_raw, now_ms());
                // 血量未知（人机）→ 不判死；真人 hp<=0 才算死
                let alive = match hp {
                    Some(h) => h > 0.0 || ai,
                    None => true,
                };
                snapshot.players.push(Player {
                    feet,
                    head,
                    health: hp.map(|h| h.round() as i32),
                    alive,
                    ptr: o,
                    vel,
                    stance: stance_height_scale(stance_enum),
                    stale: false,
                    // 人机判定只认 RoleAILogic（手填标志偏移那套已撤）
                    bot: ai,
                    team,
                    name: nm,
                    // enemy 只看队伍号：AI 也按队伍染色，队伍不同就是敌色。
                    // 自瞄是否瞄人机由独立的「瞄人机」开关决定，两条路互不干扰。
                    enemy: my_team < 0 || team != my_team,
                    side,
                    yaw,
                    stance_enum,
                    lean_smooth,
                });
                others += 1;
                if snapshot.players.len() >= MAX_PLAYERS {
                    break;
                }
            }

            // 补漏：RoleAILogic 里 gourdRoleLogic 为空的（拿不到 BattleRoleLogic），
            // 直接用 RoleAILogic 自己的 Position/Hp/Name 合成一个目标。
            let mut ai_synth = 0usize;
            // 人机姿态诊断限频（1 秒一条）
            static AI_DIAG_MS: AtomicU64 = AtomicU64::new(0);
            for (role2, a) in ai_cands.iter() {
                if *role2 >= 0x10000 {
                    continue; // 已有 BattleRoleLogic，走上面正常路径了
                }
                let Some(feet) = memory::read_vec3_contig(*a + 0x64) else {
                    continue;
                };
                if !(feet.x.is_finite() && feet.y.is_finite() && feet.z.is_finite()) {
                    continue;
                }
                if feet.x.abs() < 0.5 && feet.z.abs() < 0.5 {
                    continue;
                }
                let hp = memory::read::<f32>(*a + 0x58).filter(|h| h.is_finite() && *h >= 0.0);
                let mut nm = [0u8; 24];
                if let Some(s) = memory::read_il2cpp_string(*a + 0x128) {
                    let b = s.as_bytes();
                    let n = b.len().min(24);
                    nm[..n].copy_from_slice(&b[..n]);
                }
                let team = memory::read_il2cpp_string(*a + 0x130)
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(-1);
                let yaw = memory::read::<[f32; 4]>(*a + 0x70)
                    .map(|q| {
                        let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
                        let n = (x * x + y * y + z * z + w * w).sqrt();
                        if !n.is_finite() || n < 1e-3 {
                            0.0
                        } else {
                            (2.0 * (w * y + x * z)).atan2(1.0 - 2.0 * (y * y + z * z))
                        }
                    })
                    .unwrap_or(0.0);
                // 合成出来的人机也必须走位置平滑（否则会一步一跳 / 看着落在人后面）
                let (feet, _h, vel) = smooth_pos(*a, feet, None, now_ms());
                snapshot.players.push(Player {
                    feet,
                    head: None,
                    health: hp.map(|h| h.round() as i32),
                    alive: true, // 人机永远画
                    ptr: *a,
                    vel,
                    stance: 1.0,
                    stale: false,
                    bot: true,
                    team,
                    name: nm,
                    enemy: false,
                    side: 0,
                    yaw,
                    stance_enum: 1,
                    lean_smooth: 0.0,
                });
                ai_synth += 1;
                others += 1;
                // ==== 人机姿态诊断（限频 1 秒，只看前 3 个）====
                // 人机 = 纯服务端 AI + ClientRoleAILogic 驱动的模型：探头/蹲下这些姿态
                // 既不在 RoleAILogic.Position 里，也拿不到 headPoint，所以一探头就打不到头。
                // 这里一次把两条可能的数据源打出来定位：
                //   ① gourdRoleLogic 真有效 → 直接复用真人的 headPoint 链拿真头；
                //   ② 否则看 ClientLogic@0x40 的模型链能不能接。
                if ai_synth <= 3
                    && now_ms().saturating_sub(AI_DIAG_MS.load(Ordering::Relaxed)) >= 1000
                {
                    AI_DIAG_MS.store(now_ms(), Ordering::Relaxed);
                    let gourd = memory::read::<usize>(*a + AILOGIC_GOURD_ROLE_OFF).unwrap_or(0);
                    let client = memory::read::<usize>(*a + AILOGIC_CLIENTLOGIC_OFF).unwrap_or(0);
                    let rot =
                        memory::read::<[f32; 4]>(*a + AILOGIC_ROTATION_OFF).unwrap_or([f32::NAN; 4]);
                    let spot = memory::read_vec3_contig(*a + AILOGIC_SPOTPOINT_OFF);
                    let vis = if client >= 0x10000 {
                        memory::read::<u8>(client + CRAI_ISVISIBLE_OFF).unwrap_or(0)
                    } else {
                        0
                    };
                    let head_try = if gourd < 0x10000 {
                        "gourdRoleLogic 为空 → 只能用 RoleAILogic.Position".to_string()
                    } else if let Some(h) = read_role_head(gourd) {
                        format!("真头=({:.2},{:.2},{:.2}) ★真人链可用", h.x, h.y, h.z)
                    } else {
                        format!(
                            "gourd=0x{gourd:012X} 但 looks_like_role={} → 真人链拿不到头",
                            looks_like_role(gourd)
                        )
                    };
                    log::append(&format!(
                        "[人机诊断] ptr=0x{:012X} pos=({:.2},{:.2},{:.2}) client=0x{client:012X} vis={vis} rot=[{:.3},{:.3},{:.3},{:.3}] spot={spot:?}\n    {head_try}\n",
                        *a, feet.x, feet.y, feet.z, rot[0], rot[1], rot[2], rot[3]
                    ));
                }
                if snapshot.players.len() >= MAX_PLAYERS {
                    break;
                }
            }

            // ==== 玩家汇总日志（只在构成变化时打，避免刷屏）====
            // 用来判断“人机读得对不对”：看 AI 数量、来源、几个样本坐标。
            {
                let n_player = snapshot.players.len();
                let n_bot = snapshot.players.iter().filter(|p| p.bot).count();
                let n_enemy = snapshot.players.iter().filter(|p| p.enemy).count();
                let mut s = format!(
                    "[玩家] 共{n_player} 敌{n_enemy} AI{n_bot} | AI来源: RoleAIMgr拿到角色={ai_via_role} 直接合成={ai_synth}"
                );
                for p in snapshot.players.iter().take(4) {
                    s.push_str(&format!(
                        "\n    bot={} ({:.0},{:.0},{:.0}) p={}",
                        if p.bot { 1 } else { 0 },
                        p.feet.x,
                        p.feet.y,
                        p.feet.z,
                        p.name_str()
                    ));
                }
                s.push('\n');
                let now_s = now_ms();
                let changed = PLAYER_SUMMARY
                    .lock()
                    .map(|prev| *prev != s)
                    .unwrap_or(false)
                    && now_s.saturating_sub(SUMMARY_MS.load(Ordering::Relaxed)) >= 1000;
                if changed {
                    SUMMARY_MS.store(now_s, Ordering::Relaxed);
                    if let Ok(mut g) = PLAYER_SUMMARY.lock() {
                        *g = s.clone();
                    }
                    log::append(&s);
                }
            }

            if let Some(p) = my_pos {
                snapshot.self_pos = Some(p);
                snapshot.self_ptr = me;
                // 关键：内存自瞄/校准都靠这个静态值取自己角色，之前只读不写 → 永远=0
                LAST_SELF_PTR.store(me, Ordering::Relaxed);
            }

            if others > 0 {
                // 矩阵参照点：优先「自己(角色链)」，再补上几个敌人位置
                let mut refs: Vec<Vec3> = Vec::new();
                if let Some(p) = snapshot.self_pos {
                    refs.push(p);
                }
                for p in snapshot.players.iter().take(16) {
                    refs.push(p.feet);
                }
                refresh_matrix(snapshot, UnityPlayer_module_base, &refs);
                return;
            }
            // 角色链里只有自己 → 不 return，落到旧链兜底
        }
    }

    // ========================= ② 旧链兜底 =========================
    // （角色链只有自己 / 不可用时；老链还有别人就看得到。绝不用它写 ROLE_OBJS）
    if let Some(base) = list {
        snapshot.entity_base = base;
        let mut refs: Vec<Vec3> = Vec::new();
        if let Some(p) = self_pos {
            refs.push(p);
        }
        refresh_matrix(snapshot, UnityPlayer_module_base, &refs);
    }
}


/// 启动后台线程：
/// ① 读线程：~250Hz 调 read_into 刷新快照；
/// ② 角色线程：0.8s 首扫，之后每 400ms 刷新一次角色列表（静态链优先）。
pub fn spawn(
    unity_base: usize,
    gameassembly_base: usize,
    snapshot: std::sync::Arc<std::sync::RwLock<Snapshot>>,
) {
    let snap_read = std::sync::Arc::clone(&snapshot);
    std::thread::spawn(move || loop {
        if let Ok(mut s) = snap_read.write() {
            read_into(unity_base, gameassembly_base, &mut s);
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    });

    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(800));
        loop {
            let n = scan_roles();
            if n == 0 {
                STATIC_FAILS.fetch_add(1, Ordering::Relaxed);
            } else {
                STATIC_FAILS.store(0, Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    });
}

static STATIC_FAILS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static AUX_LOG_MS: AtomicU64 = AtomicU64::new(0);

/// 上次打印过的玩家汇总（一一样就不重复打）
static PLAYER_SUMMARY: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static SUMMARY_MS: AtomicU64 = AtomicU64::new(0);

/// 扫描日志限频（治日志刷屏）
static SCAN_LAST_LOG: AtomicU64 = AtomicU64::new(0);
static SCAN_LAST_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 兜底“全内存扫描”开关（很重，默认关）。手动补扫按钮不受此限制。
pub static HEAVY_SCAN: AtomicBool = AtomicBool::new(false);
static HEAVY_SCAN_LAST: AtomicU64 = AtomicU64::new(0);
const HEAVY_SCAN_INTERVAL_MS: u64 = 10_000;

// ===================== 手动“全内存补扫角色” =====================
// 飞机/出生岛阶段 StartGame.RoleList 里只有自己，权威链拿不到别人。
// 这里允许手动全内存扫一次 BattleRoleLogic：结果会一直并入角色列表（位置仍然实时读）。
// 很慢（几秒），所以只在你手动点的时候跑。

