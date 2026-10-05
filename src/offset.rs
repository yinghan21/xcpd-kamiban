//! 全局偏移表 —— 所有硬编码的内存偏移都集中在这一个文件里。
//!
//! 游戏更新/偏移对不上时，只改这里，不用在 data.rs / memory.rs / esp.rs 里到处翻。
//! 约定：
//!   * 带 `RVA` 的是 **UnityPlayer.dll 的静态地址**（要加模块基址再用）；
//!   * 其余是**结构体字段偏移**（配指针直接用）；
//!   * 每条都写了实测来源，别凭 dump 猜 —— dump 里的字段名和实际布局常对不上
//!     （例如 `BattleRoleLogic` 的坐标是 `$t // 0x7F8`，而 dump 里的 `$A // 0xA4` 读出来是常量 (0,1,0)）。
//!
//! 历史遗留的候选偏移（实测对不上的）已经清掉，不再留在表里误导。
#![allow(dead_code)]

// ===================== ① UnityPlayer.dll 静态链 =====================

/// 玩家实体表表头（RVA）。旧链：ENTITY_LIST → +0x48 → +0x0 → +0x60 = 实体指针数组。
/// 现在主力走“按类名枚举角色”，这条只作回退/诊断用。
pub const ENTITY_LIST: usize = 0x01C4B840;
/// 视图矩阵（RVA）。链：VIEW_MATRIX → +0x0 → +0x40 → +0x100 = [f32; 16]
pub const VIEW_MATRIX: usize = 0x01C1ACC0;
pub const VIEW_MATRIX_OFFSET_1: usize = 0x0;
pub const VIEW_MATRIX_OFFSET_2: usize = 0x40;
pub const VIEW_MATRIX_OFFSET_3: usize = 0x100;

/// ENTITY_LIST 旧链的第 1 跳
pub const OFFSET_X: usize = 0x48;
pub const OFFSET_X_OFFSET_1: usize = 0x0;
/// 旧链第 3 跳 = 实体指针数组本身
pub const OFFSET_X_OFFSET_2: usize = 0x60;

/// 实体自身的“落地位置”字段（战场场景实测 +0x15C）：跳跃/腾空时它不动，
/// 要跟跳就用 LIVE_POS_PTR 那条实时链。
pub const ENTITY_POS_OFFSET: usize = 0x15C;

// ===================== ② 角色 BattleRoleLogic =====================

/// 角色类名（按类名枚举角色时用；来自 Il2CppDumper 的 dump.cs）
pub const ROLE_CLASS_NAME: &str = "BattleRoleLogic";
/// 角色坐标（Vector3）。实证：是 `private Vector3 $t; // 0x7F8`（连排布局）
pub const ROLE_POS_OFF: usize = 0x7F8;
/// 姿态枚举 `RoleData.AC_StandState // 0x218`（暂未使用，姿态比例走 STANCE_OFF）
#[allow(dead_code)]
pub const ROLE_STAND_OFF: usize = 0x218;
/// 昵称 string*（`public string NickName; // 0x770`）
pub const ROLE_NAME_OFF: usize = 0x770;
/// 队伍号 long（`public long TeamNum; // 0x7B0`）。实测取值 0/1/2，**0 是合法队号**
pub const ROLE_TEAM_OFF: usize = 0x7B0;
/// 是否 AI：`mServerRoleData`（内嵌 ServerRoleData @0x270）的 `IsRobot` @+0x310 → 绝对 0x580
pub const ROLE_ROBOT_OFF: usize = 0x580;
/// 是否在线（`public bool isOnline; // 0xB24`）。
/// 退出/传送走后角色对象仍留在 StartGame.RoleList 里、位置冻结 → “幽灵框”的根源。
pub const ROLE_ONLINE_OFF: usize = 0xB24;
/// 备用昵称：ServerRoleData.NickName @+0x8 → 绝对 0x278（主昵称 0x770 为空时用）
pub const ROLE_NAME2_OFF: usize = 0x278;
/// 角色朝向四元数（`public Quaternion NowRotation; // 0x7E8`）。
/// 内存自瞄“模式 B”往这里写，用来现场验证相机是否跟随角色朝向。
pub const ROLE_NOWROT_OFF: usize = 0x7E8;

// ===================== ③ RoleNet / RoleNetMirror =====================

/// 角色 → RoleNet
pub const ROLE_NET_OFF: usize = 0x740;
/// RoleNet → mirror
pub const ROLE_MIRROR_OFF: usize = 0x90;
/// 血量链：角色+0x740(RoleNet) → +0x90(RoleNetMirror) → SyncHp @0x168（float，满 100）
pub const ROLE_HP_OFF: usize = 0x168;
/// 真实姿态枚举：RoleNetMirror.SyncStandbyState @0x194（Stand=1 / Crouch=2 / Prone=3）
pub const ROLE_STANCE_OFF: usize = 0x194;
/// RoleNet.roleNetClient @0x58（内含 isLocalPlayer @+0x1D = “这是不是自己”）
pub const ROLE_NETCLIENT_OFF: usize = 0x58;
/// RoleNet.$c = RoleAILogic @0x80（非空 = 人机/AI）。
/// ⚠️ 香肠派对的人机不走 RoleNet（本地没有 BattleRoleLogic），这条链对人机是空的，
/// 人机识别实际靠 StartGame.MyRoleAIMgr 那条（见 data::ai_candidates）。
pub const ROLE_AILOGIC_OFF: usize = 0x80;
/// RoleNetMirror.SyncNickName @0xB8 / SyncTeamNum @0xA8
pub const MIRROR_NICK_OFF: usize = 0xB8;
pub const MIRROR_TEAM_OFF: usize = 0xA8;
/// RoleNetMirror.SyncSideAim（服务器同步的侧身/探头值，int）
pub const MIRROR_SIDEAIM_OFF: usize = 0x1A0;
/// RoleNetMirror.SyncTopShoot（顶射）
pub const MIRROR_TOPSHOOT_OFF: usize = 0x1A4;
/// RoleNetMirror.SyncStandbyState（姿态枚举：Stand=1 / Crouch=2 / Prone=3 ...）
pub const MIRROR_STANCE_OFF: usize = 0x194;

// ===================== ④ RoleAILogic（人机） =====================

/// RoleAILogic.Hp @0x58（人机常年读到 0 = “服务端没下发”，0 当未知、不当死）
pub const AILOGIC_HP_OFF: usize = 0x58;
/// RoleAILogic.Name @0x128
pub const AILOGIC_NAME_OFF: usize = 0x128;
/// RoleAILogic.TeamId @0x130（string；“0” 也是合法队号，所以解析不出时要退回 -1）
pub const AILOGIC_TEAM_OFF: usize = 0x130;
/// RoleAILogic.Position @0x64（Vector3）—— 合成人机的坐标来源（服务端同步，低频阶梯式）
pub const AILOGIC_POS_OFF: usize = 0x64;
/// RoleAILogic.gourdRoleLogic @0x1F0 —— 指回它的 BattleRoleLogic。
/// 为空 = 这人机在本地没有战斗逻辑对象，只能靠 RoleAILogic 自己合成。
pub const AILOGIC_GOURD_ROLE_OFF: usize = 0x1F0;
/// RoleAILogic.Rotation @0x70（Quaternion）—— 服务端同步的朝向（只有 yaw，探头的侧倾不在这）
pub const AILOGIC_ROTATION_OFF: usize = 0x70;
/// RoleAILogic.ClientLogic @0x40 → ClientRoleAILogic。人机的**模型**是它在驱动的
/// （服务端只管逻辑，探头/蹲下这些动画姿态只在客户端侧）。
pub const AILOGIC_CLIENTLOGIC_OFF: usize = 0x40;
/// RoleAILogic.SpotPoint @0x1C0（Vector3）—— 候选：人机的驻守/瞄准点
pub const AILOGIC_SPOTPOINT_OFF: usize = 0x1C0;

// ---- ClientRoleAILogic（人机客户端模型控制器）----
// 来自 _crai.txt 的类 dump。人机没有 BattleRoleLogic，所以真人的
// `RoleControl.headPoint` 链（角色+0xAF0 起步）对人机不适用，模型头只能从这边找。
/// ClientRoleAILogic.RoleAITrans @0x60（Unity Transform）—— 人机模型根节点
pub const CRAI_ROLEAITRANS_OFF: usize = 0x60;
/// ClientRoleAILogic.state @0x70（ClientRoleAIState）
pub const CRAI_STATE_OFF: usize = 0x70;
/// ClientRoleAILogic.animator @0x78（ClientRoleAIAnimator）—— 探头/蹲下动画应该在这条链里
pub const CRAI_ANIMATOR_OFF: usize = 0x78;
/// ClientRoleAILogic.controller @0x80（ClientRoleAIController）
pub const CRAI_CONTROLLER_OFF: usize = 0x80;
/// ClientRoleAILogic.isVisible @0xD5
pub const CRAI_ISVISIBLE_OFF: usize = 0xD5;
/// ClientRoleAILogic.TeamIndex @0xF4（int）
pub const CRAI_TEAMINDEX_OFF: usize = 0xF4;
/// StartGame.MyRoleAIMgr @0x298 → RoleAIManager
pub const STARTGAME_AIMGR_OFF: usize = 0x298;
/// RoleAIManager.logicList @0x18 → List<RoleAILogic>（人机列表；人机常常不在 StartGame.RoleList）
pub const AIMGR_LOGICLIST_OFF: usize = 0x18;

// ===================== ⑤ 实体级：实时位置 / 姿态 / 头部候选 =====================

/// 实时位置（跳跃/腾空会跟随）的访问路径：
/// 实体 +0x1E8 = 指向“位置对象”的指针；位置对象里 +0xA0 开始是连续的 (x, y, z)。
/// 实体自身的 +0x15C 是“落地位置”，跳的时候不动。
pub const LIVE_POS_PTR: usize = 0x1E8;
pub const LIVE_POS_OFF: usize = 0xA0;

/// 姿态字段：实测站立 ≈0.65、蹲下/趴下 ≈0.30（用“蹲下差异”扫出来的）
pub const STANCE_OFF: usize = 0x264;

/// 头部候选所在的“子对象”指针：实体 +0x110 → 子对象
pub const HEAD_CHILD_PTR: usize = 0x110;
/// 实测路径：实体+0x110 → 子+0x138 → 孙，孙的 +0xA0 就是头顶
pub const HEAD_MID_PTR: usize = 0x138;
pub const HEAD_POS_OFF: usize = 0xA0;

// ===================== ⑥ 客户端模型链（真头 / 身高 / 探头动画） =====================
//   BattleRoleLogic +0xAF0 → RoleLogicClient
//   RoleLogicClient +0x80  → BattleRole（RoleClient）
//   BattleRole      +0x240 → RoleControl（MonoBehaviour，模型控制）
//   RoleControl     +0x16C → Vector3 headPoint（游戏自己的 GetHeadPoint()）
//   RoleControl（基类 RoleControlBase）+0x28 身高 / +0x20 中心Y / +0x2C 半径
//   BattleRole      +0x20  → List<BattleComponent>（里面有 BattleRoleCameraComponent._headPoint @0x58）

pub const ROLE_LOGICCLIENT_OFF: usize = 0xAF0;
pub const RLC_BATTLEROLE_OFF: usize = 0x80;
pub const BATTLE_ROLECONTROL_OFF: usize = 0x240;
/// 真头来源：RoleControl.headPoint（蹲下/探头会跟着动，所以不要再乘姿态比例）
pub const ROLECONTROL_HEAD_OFF: usize = 0x16C;
pub const ROLECONTROL_SIZE_H_OFF: usize = 0x28;
pub const ROLECONTROL_SIZE_CY_OFF: usize = 0x20;
pub const ROLECONTROL_SIZE_R_OFF: usize = 0x2C;
pub const BATTLE_COMPONENTS_OFF: usize = 0x20;
pub const BR_CAMERA_HEADPOINT_OFF: usize = 0x58;
/// RoleControl.animatorControl（AnimatorControl）：Head 骨骼 @0xE8，侧身动画值 @0x514
pub const ROLECONTROL_ANIMCTRL_OFF: usize = 0x48;
pub const ANIMCTRL_HEAD_OFF: usize = 0xE8;
pub const ANIMCTRL_TOP_OFF: usize = 0x520;
pub const ANIMCTRL_NOWSIDE_OFF: usize = 0x514;
pub const ANIMCTRL_ENDSIDE_OFF: usize = 0x518;

// ===================== ⑦ 移动速度链（自身加速 / 写内存） =====================
// 这游戏有专门的反作弊（`GoGoPartyCheatModeData.SendMoveSpeedCheat` / `FastGoldDashCheatMove`），
// 改速度有被判定/处罚的风险；倍数建议别超 1.5~2.0。

/// `BattleRole.myRoleMover`（RoleCapsuleMove，真正走路用的胶囊控制器）
pub const BATTLE_ROLEMOVER_OFF: usize = 0x4A0;
pub const ROLECAPSULE_SPEED_OFF: usize = 0xF8;
/// BattleRoleLogic.BuffAddMoveSpeed (float)
pub const ROLE_BUFF_ADD_SPEED_OFF: usize = 0x7A0;
/// BattleRoleLogic.GameHubMoveSpeedRatio (float)
pub const ROLE_GAMEHUB_RATIO_OFF: usize = 0xBE0;
/// WarData.RoleMoveSpeed（static float）
pub const WARDATA_ROLEMOVESPEED_OFF: usize = 0x90;
/// BattleRoleLogic.MyRoleBaseData（SORoleBasePCData）
pub const ROLE_MYROLEBASEDATA_OFF: usize = 0x718;
pub const SRBD_MOVE_SPEED_OFF: usize = 0x1DC;
pub const SRBD_MOVE_RUN_SPEED_OFF: usize = 0x1E8;
pub const BATTLE_ROLESPEED_OFF: usize = 0xAAC;

// ===================== ⑧ 视角写入（内存自瞄） =====================
// RoleControl.y_camRot @0x78（偏航/yaw）、x_camRot @0x7C（俯仰/pitch）—— 本地角色的视角角度。
// 不走鼠标，直接把角度写到目标方向 → 瞬间锁人。
// ⚠️ 有 AnalyzeAimbot 之类的检测，瞬间对齐视角是“教科书式”的异常，风险自负。
pub const ROLECONTROL_CAM_YAW_OFF: usize = 0x78;
pub const ROLECONTROL_CAM_PITCH_OFF: usize = 0x7C;
