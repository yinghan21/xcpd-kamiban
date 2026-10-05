use crate::memory::Projection;

/// 自瞄部位
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SnapTarget {
    Head,
    Feet,
}

pub struct Config {
    ///菜单
    pub menu_open: bool,
    ///透视ESP
    pub esp_enabled: bool,
    ///画框
    pub draw_box_enabled: bool,
    ///画线
    pub draw_snapline: bool,
    ///画血
    pub draw_health_bar: bool,
    ///自瞄开关
    pub aim_enabled: bool,
    ///自瞄FOV
    pub aim_fov: f32,
    ///自瞄平滑
    pub aim_smooth: f32,
    ///自瞄速度倍率(对应鼠标)
    pub aim_speed: f32,
    /// 自瞄按键：true = 按住右键才自瞄；false = 开关一开就一直瞄
    pub aim_hold_mode: bool,
    /// 自瞄部位（头 / 脚）
    pub aim_target: SnapTarget,
    /// 自瞄点在“框顶”基础上再上/下移多少米（负=往下，瞄胸/腿用）
    pub aim_offset_y: f32,
    /// 自喵触发键（VK 码，默认 2 = 鼠标右键）；菜单里点“按键”那行后按新键就能改。
    pub aim_key: i32,
    /// 粘滞阈值（像素）：上一帧目标比“离准心最近的那个”多偏超过这个值就换人。
    /// 0 = 永远喵离准心最近的。
    pub aim_stick_px: f32,
    /// 只瞄敌人：队友不瞄（默认开）。
    /// ⚠️ 人机（AI）在 data.rs 里 enemy 恒为 false（人机不参与队伍阵营判定），
    /// 所以开着这个开关时，人机要靠下面的 aim_bots 单独放行，否则人机模式一个目标都锁不到。
    pub aim_enemy_only: bool,
    /// 瞄人机（AI/bot）：人机模式必须开。默认开。
    /// 人机不走队伍阵营判定（enemy 恒 false），只能靠这个开关放行。
    pub aim_bots: bool,
    /// 隐藏离线玩家：退出/传送走后角色对象仍留在 RoleList（位置冻结）→ 幽灵框。
    /// 用 BattleRoleLogic.isOnline(+0xB24) 判断（默认开）。
    pub hide_offline: bool,
    /// 幽灵过滤：角色位置长期冻结（退出/传送后对象还留在 RoleList）就隐藏。
    pub ghost_filter: bool,
    /// 幽灵判定阈值（秒）：位置不动超过这么久就当幽灵。
    pub ghost_freeze_s: f32,
    /// 屏幕空间超前补偿（帧）：治“自喵跟在移动敌人后面”。0 = 关。
    pub aim_screen_lead: f32,
    /// 位置平滑：治“框跟随一卡一卡”（人机/低频同步的坐标是步进式的）。
    pub pos_smooth: bool,
    /// 位置平滑时间常数（毫秒）。
    pub pos_smooth_ms: f32,
    /// 自瞄预判：固定的提前量（秒，0 = 不预判）
    pub aim_lead_time: f32,
    /// 忽略自己（备用；现在自己已经不在玩家列表里了）
    pub ignore_self: bool,

    // ================= 校准用 =================
    // 矩阵/坐标对不上时，用这几个开关现场试，不用重编译。
    ///矩阵按行主序(转置)解释
    pub matrix_row_major: bool,
    ///屏幕 X 取反
    pub flip_x: bool,
    ///屏幕 Y 取反
    pub flip_y: bool,
    ///坐标布局: 0=8字节间隔(原始) 1=连续(x,y,z) 2=连续(x,z,y)
    pub layout: usize,
    ///身高（世界单位）：读不到真实头时，头顶 = 脚坐标 + 这个值。负数 = 高度轴反了。
    pub player_height: f32,
    /// 头顶微调（米）：读到真实头后再往上加一点，让框顶刚好盖住头顶。
    pub head_pad: f32,
    /// 用真头：走客户端链 `RoleLogicClient → BattleRole → RoleControl.headPoint @0x16C`
    /// （实测 dY≈1.26、水平差 0、蹲下会跟着降）。关掉则一律走“脚 + 身高”。
    pub use_real_head: bool,
    /// 探头/侧身跟随：用 SyncSideAim × 朝向把头顶横向挪出去（解决“敌人探头锁不到头”）。
    pub lean_follow: bool,
    /// 探头横向偏移量（米）：一个侧身大概把头探出去多远，自己微调到框贴头。
    pub lean_shift_m: f32,
    /// 探头方向反转（如果发现补偿方向反了就打勾）。
    pub lean_invert: bool,
    /// 趴下时头部前移量（米）：趴下的头在身体前方，headPoint 不给前移。
    pub prone_fwd_m: f32,
    /// 蹲下时头部前移量（米）。
    pub crouch_fwd_m: f32,
    /// 探头平滑：跟模型探头动画同步（避免瞬移导致挠点跑到头前面）。
    pub lean_smooth_on: bool,
    /// 尝试读取真实头部。
    /// 蹲下/趴下跟随：用实体 +0x264 的姿态字段按比例把框压矮
    /// （实测站立 ≈0.65、蹲下/趴下 ≈0.30）
    pub crouch_follow: bool,
    /// 要不要给自己画那个洋红色的框（默认不要，免得挡住视线）
    pub draw_self_box: bool,
    /// 方框世界宽度，单位米（人物身体宽度的近似）
    pub box_width: f32,
    /// 方框屏幕内边距，单位像素
    pub box_pad: f32,
    /// 屏幕顶部显示「绘制 X 人 / 背敌 Y 人」
    pub draw_counter: bool,
    /// 在框上方画昵称（BattleRoleLogic.NickName @0x770）
    pub draw_name: bool,
    /// 敌我区分：按队伍号(TeamNum @0x7B0)把队友画成另一种颜色
    pub team_check: bool,
    /// 骨架（棍人）：只靠“脚 + 头顶”两点推算，跟着蹲下/跳起缩放。
    pub draw_skeleton: bool,
    /// 实时高度微调（米）：实时位置是角色根节点，比脚底略高，这里减去这个值。
    pub live_y_adjust: f32,
    ///手动地址（十六进制，可带 0x）：填了就只画这个地址读出的位置，用来验证 CE 里找到的地址。
    pub manual_addr: String,
    /// 用“按类名枚举角色(BattleRoleLogic)”代替坏掉的实体表链（推荐开）
    pub use_role_scan: bool,
}

impl Config {
    pub fn projection(&self) -> Projection {
        Projection {
            row_major: self.matrix_row_major,
            flip_x: self.flip_x,
            flip_y: self.flip_y,
        }
    }

    /// 解析手动地址；为空或解析失败返回 None。
    pub fn manual_addr_value(&self) -> Option<usize> {
        let s = self.manual_addr.trim();
        let s = s
            .strip_prefix("0x")
            .or_else(|| s.strip_prefix("0X"))
            .unwrap_or(s)
            .replace('_', "");
        if s.is_empty() {
            return None;
        }
        let v = usize::from_str_radix(&s, 16).ok()?;
        if v == 0 {
            None
        } else {
            Some(v)
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            //菜单
            menu_open: true,
            //透视ESP
            esp_enabled: true,
            //画框
            draw_box_enabled: true,
            //画线
            draw_snapline: false,
            //画血
            draw_health_bar: false,
            //自瞄开关
            aim_enabled: false,
            //自瞄FOV
            aim_fov: 150.0,
            //自瞄平滑
            aim_smooth: 2.0,
            //自瞄速度倍率(对应鼠标)
            aim_speed: 1.0,
            aim_hold_mode: true,
            aim_target: SnapTarget::Head,
            aim_offset_y: 0.1,
            aim_key: 2,
            aim_stick_px: 8.0,
            aim_enemy_only: true,
            // 人机模式必开：AI 的 enemy 恒 false，不单独放行就完全锁不到人机
            aim_bots: true,
            hide_offline: true,
            ghost_filter: false,
            ghost_freeze_s: 8.0,
            aim_screen_lead: 1.5,
            pos_smooth: true,
            pos_smooth_ms: 60.0,
            aim_lead_time: 0.0,
            ignore_self: true,
            // 校准相关默认值（当前行为：列主序 + 不翻转 + 8 字节间隔）
            matrix_row_major: false,
            flip_x: false,
            flip_y: false,
            layout: 1,
            player_height: 1.5,
            head_pad: 0.20,
            use_real_head: true,
            lean_follow: true,
            lean_shift_m: 0.55,
            lean_invert: false,
            lean_smooth_on: true,
            prone_fwd_m: 0.4,
            crouch_fwd_m: 0.15,
            crouch_follow: true,
            draw_self_box: false,
            box_width: 0.5,
            box_pad: 1.5,
            draw_counter: true,
            draw_name: true,
            team_check: true,
            draw_skeleton: false,
            live_y_adjust: 0.8,
            manual_addr: String::new(),
            use_role_scan: true,
        }
    }
}
