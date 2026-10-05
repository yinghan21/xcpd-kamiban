# 从零写一个 Unity IL2CPP 游戏的 ESP + 自瞄（Rust 实战教程）

> 这份文档来自一个真实项目：**香肠派对（Sausage Man，Unity + IL2CPP）** 的 ESP 透视 + 鼠标自瞄外挂，用 Rust 写成、编译成 DLL 注入。
>
> 它记录的不是"最终代码长什么样"，而是**每一行为什么这么写**——包括所有踩过的坑、试错过的错误方向、以及判断"这条路走不通"的信号。
>
> 目标：读完你能自己写出同类项目，**并且能把这套方法迁移到别的游戏**。

---

## 目录

- [第 0 章 原理：你真正要解决的只有三个问题](#第-0-章-原理你真正要解决的只有三个问题)
- [第 1 章 工程骨架](#第-1-章-工程骨架)
- [第 2 章 挂上渲染：Present Hook](#第-2-章-挂上渲染present-hook)
- [第 3 章 内存读写与崩溃防护](#第-3-章-内存读写与崩溃防护)
- [第 4 章 IL2CPP：别硬扫内存](#第-4-章-il2cpp别硬扫内存)
- [第 5 章 找到角色列表：三条路](#第-5-章-找到角色列表三条路)
- [第 6 章 挖偏移：把 dump 当线索，不当真相](#第-6-章-挖偏移把-dump-当线索不当真相)
- [第 7 章 世界坐标 → 屏幕](#第-7-章-世界坐标--屏幕)
- [第 8 章 ESP 绘制](#第-8-章-esp-绘制)
- [第 9 章 自瞄](#第-9-章-自瞄)
- [第 10 章 边界情况：探头、人机、幽灵框](#第-10-章-边界情况探头人机幽灵框)
- [第 11 章 工程纪律：让你能持续维护它](#第-11-章-工程纪律让你能持续维护它)
- [第 12 章 迁移到其他游戏的 Checklist](#第-12-章-迁移到其他游戏的-checklist)
- [第 13 章 排查表：症状 → 原因 → 动作](#第-13-章-排查表症状--原因--动作)
- [附录](#附录)

---

# 第 0 章 原理：你真正要解决的只有三个问题

## 0.1 一条数据链看懂全局

外挂的本质是：**读游戏的内存 → 算出你该看什么/打哪 → 画出来/动鼠标**。拆开就是这条链：

```
【目标进程的内存空间】
│
├─ GameAssembly.dll           ← IL2CPP 把 C# 编译成的原生代码 + 全部元数据都在这里
│    │
│    ├─ Il2CppClass（类元数据）
│    │    ├─ 类名字符串         "BattleRoleLogic"
│    │    ├─ 字段偏移表         "位置在对象+0x7F8"
│    │    └─ 静态字段值         "StartGame 实例的指针"
│    │
│    └─ 角色对象实例 × N
│         ├─ +0x7F8  Vector3 位置（世界坐标）
│         ├─ +0x770  string* 昵称指针
│         ├─ +0x7B0  long    队伍号
│         └─ +0x740  RoleNet* → +0x90 → +0x168 float 血量
│
└─ UnityPlayer.dll
     └─ 视图投影矩阵（16 个 float）  ← 世界坐标转屏幕的关键

【你注入进去的 DLL】
① 读内存 ────────→ 拿到所有敌人的「世界坐标」
② 乘矩阵 ────────→ 转成「屏幕坐标」（像素）
③ 画框 / 算位移 ─→ ImGui 画 ESP 框；SendInput 移鼠标
```

**关键洞察**：整件事你只需要解决三个问题——

1. **怎么找到人？**（角色列表在哪、位置字段偏移多少）
2. **怎么把人投影到屏幕？**（矩阵在哪、怎么乘）
3. **怎么把准心移过去？**（鼠标位移怎么算）

剩下 90% 的工作量全是工程细节：防抖、防崩、边界情况、版本适配。**先把这三个打通，项目就活了**，别一开始就追求完美。

## 0.2 为什么用内存读取，而不是图像识别

| 方案 | 优点 | 致命缺点 |
|---|---|---|
| **内存读取**（本项目） | 零延迟、不受遮挡影响、算力开销极低、能拿到血量和队伍 | 偏移随游戏更新失效；有反作弊风险 |
| 图像识别（YOLO 之类） | 不碰内存，理论上难检测 | 延迟高（几十 ms）、遮挡就瞎、吃 GPU、认不出队友 |

逆向场景里 **内存读取是主流**，因为延迟低到"和游戏同帧"，而且能拿到屏幕上看不见的信息（血量、朝向、是否队友）。

## 0.3 技术选型

| 需求 | 选择 | 为什么 |
|---|---|---|
| 语言 | **Rust** | 无 GC（自己写的东西不能在游戏里引起卡顿）；编译产物是单个 DLL；`Option` 让"读内存可能失败"这件事在类型层面强制处理 |
| 渲染挂钩 | **hudhook** | 现成的 Present hook + ImGui + DX11 后端全包了。自己写这套要上千行，还得处理各种 DXGI 版本 |
| UI | **imgui**（hudhook 内置） | 游戏内 UI 的行业标准；画 ESP 框用的 draw list 效率高 |
| Win32 API | **windows-sys** | 微软官方绑定，无中间层，也不需要 C++ 工具链 |

> **注意**：不同游戏的反作弊强度差别巨大。本项目目标（手游移植 PC 端）反作弊相对宽松。换成带内核级反作弊的游戏（EAC/BE/Vanguard），注入这一关就过不去，那不是本文档的范畴。

---

# 第 1 章 工程骨架

## 1.1 Cargo.toml

```toml
[package]
name = "xcpd"          # 编译产物会是 xcpd.dll
version = "0.1.0"
edition = "2024"

[lib]
# ★ 必须是 cdylib —— 我们要的是一个能被注入器 LoadLibrary 加载的原生 DLL，
#   而不是 Rust 默认的 rlib（那是给别的 Rust 代码链接用的）。
crate-type = ["cdylib"]

[dependencies]
imgui = "0.12.0"       # 画 UI 和 ESP 框
hudhook = "0.9.0"      # Present hook + ImGui 后端集成
anyhow = "1.0"         # 错误处理

[dependencies.windows-sys]
version = "0.61.0"
# ★ features 必须显式列出用到的模块 —— windows-sys 是拆成 feature 的，
#   不写的话对应函数根本不存在（编译报 "cannot find function"）。
features = [
    "Win32_System_SystemServices",       # DLL_PROCESS_ATTACH
    "Win32_UI_WindowsAndMessaging",      # FindWindow / ClipCursor
    "Win32_Foundation",                  # HWND / RECT
    "Win32_UI_Input_KeyboardAndMouse",   # GetAsyncKeyState / SendInput
    "Win32_System_Memory",               # ReadProcessMemory / VirtualQuery
    "Win32_System_LibraryLoader",        # GetModuleHandle / GetProcAddress
    "Win32_System_Threading",            # GetCurrentProcess
    "Win32_System_Diagnostics_Debug",    # ReadProcessMemory
]

# ★ 关键：dev profile 也开 opt-level 2。
#   原因：内存扫描是 ms 级轮询循环，每个 ESP 都吃 CPU。
#   用默认的 opt-level 0 会让游戏明显掉帧 —— 玩家会立刻发现"开了这个东西就卡"。
[profile.dev]
opt-level = 2
```

## 1.2 模块划分

```
src/
├── lib.rs       入口：DllMain / hook 初始化 / 渲染循环 / 菜单     ← 与游戏无关的框架
├── memory.rs    内存读写 + IL2CPP API + 矩阵数学                 ← 与游戏无关的底座
├── offset.rs    ★ 所有硬编码偏移集中在这里
├── config.rs    菜单配置项（结构体）+ 默认值
├── data.rs      读游戏数据：角色枚举、快照构建、位置平滑          ← 游戏相关
├── esp.rs       画框 / 骨架 / 血条 / 昵称
├── aim.rs       自瞄：目标选择 + 鼠标位移
└── log.rs       限流日志
```

**这个划分的核心原则**：**把"与游戏无关的"和"与游戏相关的"分开**。

- `memory.rs` / `lib.rs` / `log.rs` 换个游戏几乎不用改
- `data.rs` / `offset.rs` / `esp.rs` / `aim.rs` 是游戏相关的部分

**迁移到新游戏时，你只需要重写后者** —— 这是这份文档最想让你带走的结构。

## 1.3 为什么 offset.rs 要单独存在

游戏一更新，偏移就变。如果把 `0x7F8` 这种数字散落在十几个文件里，改起来是灾难。所以：

```rust
//! 全局偏移表 —— 所有硬编码的内存偏移都集中在这一个文件里。
//!
//! 约定：
//!   * 带 `RVA` 的是 UnityPlayer.dll 的静态地址（要加模块基址再用）；
//!   * 其余是结构体字段偏移（配指针直接用）；
//!   * 每条都写了实测来源 —— 别凭 dump 猜，dump 里的字段名和实际布局常对不上。

// ===================== ① UnityPlayer.dll 静态链 =====================

/// 视图矩阵（RVA）。链：VIEW_MATRIX → +0x0 → +0x40 → +0x100 = [f32; 16]
pub const VIEW_MATRIX: usize = 0x01C1ACC0;
pub const VIEW_MATRIX_OFFSET_1: usize = 0x0;
pub const VIEW_MATRIX_OFFSET_2: usize = 0x40;
pub const VIEW_MATRIX_OFFSET_3: usize = 0x100;

// ===================== ② 角色 BattleRoleLogic =====================

/// 角色类名（按类名枚举角色时用）
pub const ROLE_CLASS_NAME: &str = "BattleRoleLogic";

/// 角色坐标（Vector3）。
/// ★ 实证：是 `private Vector3 $t; // 0x7F8`（连排布局），
///   而**不是** dump 里那个显眼的 `$A // 0xA4` —— 那个读出来是常量 (0,1,0)。
///   这条注释就是它的价值：下次有人照着 dump 写 0xA4，看一眼这里就知道为什么不行。
pub const ROLE_POS_OFF: usize = 0x7F8;

/// 队伍号 long。★ 实测取值 0/1/2，**0 是合法队号** ——
///   写代码时千万别把 0 当"未设置"来用（我们踩过这个坑，见第 10 章）。
pub const ROLE_TEAM_OFF: usize = 0x7B0;
```

`data.rs` 里用 `pub use crate::offset::*;` 重新导出，这样：

- 老代码里写 `crate::data::ROLE_POS_OFF` 的地方**一行都不用改**
- 文件内 `offset::ROLE_POS_OFF` 的老写法也照旧能用
- 改偏移时只动一个文件

---

# 第 2 章 挂上渲染：Present Hook

## 2.1 为什么必须挂钩

游戏每帧会把画面"提交"给显卡，DX11 下的调用是 `IDXGISwapChain::Present`。你要画 ESP，就得在**它之后、而且是在同一帧里**画——否则会出现：

- 用独立窗口画 → 无法和游戏画面对齐，全屏下还会闪
- 用普通定时器画 → 和游戏帧率不同步，框会抖/延迟

**挂钩 Present 之后**，你的绘制代码就在游戏的渲染线程里、每帧执行一次，天然同步。

## 2.2 最小可用的接入代码

```rust
use hudhook::hooks::dx11::ImguiDx11Hooks;
use hudhook::{Hudhook, ImguiRenderLoop, MessageFilter, RenderContext};
use imgui::{Condition, Ui, *};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

/// 渲染循环的状态。hudhook 每帧会调它的 render()。
struct EspLoop {
    config: Config,                          // 菜单里的所有开关/滑块
    snapshot: Arc<RwLock<Snapshot>>,          // 读线程写、渲染线程读的共享快照
    prev_insert_down: bool,                   // 上一次 Insert 键的状态（用于边沿检测）
}

impl ImguiRenderLoop for EspLoop {
    /// 字体必须在 initialize 里加 —— hudhook 紧接着就会 build 字体图集，
    /// 之后再加就晚了（中文会显示成方块）。
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _rc: &'a mut dyn RenderContext) {
        load_chinese_font(ctx);
    }

    fn before_render<'a>(&'a mut self, ctx: &mut Context, _rc: &'a mut dyn RenderContext) {
        let io = ctx.io_mut();

        // 菜单开着时才让鼠标光标显示出来（否则游戏里一直有个鼠标指针很奇怪）
        io.mouse_draw_cursor = self.config.menu_open;

        // ★ 关键：ImGui 需要知道屏幕分辨率才能把框画对位置。
        //   不设的话，hudhook 默认尺寸可能和实际游戏分辨率不一致 → 框全部错位。
        if let Some((w, h)) = memory::game_client_size() {
            io.display_size = [w, h];
        }

        // 菜单开着时解除鼠标锁定（游戏通常把光标锁在窗口中心）
        if self.config.menu_open {
            let _ = unsafe { ClipCursor(std::ptr::null()) };
        }
    }

    fn render(&mut self, ui: &mut Ui) {
        // ★ 每帧打一次时间戳：告诉内存模块"游戏现在能正常渲染，元数据是稳定的"。
        //   加载/切场景时 Present 不会跑，时间戳自然变旧，读线程就会自动停手。
        //   这一行是防闪退的关键，详见第 3 章。
        memory::api_tick();

        // 正在等用户按一个键 → 把它捕获为自瞄按键
        if let Some(vk) = aim::poll_bind_key() {
            self.config.aim_key = vk;
        }
        self.poll_menu_key();                    // Insert 开关菜单

        // 拿快照。★ 用 read() 而不是直接 unwrap：读线程可能正在写，
        //   再说不加锁读一份旧数据也完全够用（画一帧而已）。
        let snapshot = match self.snapshot.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),   // 读线程 panic 过也别整个挂掉
        };

        // 把菜单里的值同步给读线程用的全局原子变量。
        // ★ 为什么用原子变量而不是直接共享 Config？
        //   因为读线程和渲染线程是两条独立线程，Config 有几十个字段，
        //   加锁会互相拖累。这些值都是 Copy 的小类型，用原子变量最省事。
        data::USE_REAL_HEAD.store(self.config.use_real_head, Ordering::Relaxed);
        data::HIDE_OFFLINE.store(self.config.hide_offline, Ordering::Relaxed);
        // ...（其余同理）

        esp::draw(ui, &self.config, &snapshot);   // 画 ESP
        draw_menu(ui, &mut self.config);          // 画菜单
    }

    /// 菜单开着时，把鼠标/键盘输入从游戏手里抢过来，否则点菜单会操作到游戏。
    fn message_filter(&self, io: &Io) -> MessageFilter {
        if !self.config.menu_open {
            return MessageFilter::empty();        // 菜单没开 → 什么都不过滤，游戏正常操作
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
```

## 2.3 入口：DllMain

```rust
#[unsafe(no_mangle)]
#[allow(non_snake_case)]
extern "system" fn DllMain(module: *mut c_void, call_reason: u32, _: *mut c_void) -> bool {
    if call_reason == DLL_PROCESS_ATTACH {
        let module_raw = module as usize;

        // ★ 为什么必须 spawn 一个线程，而不是直接在这里初始化？
        //   DllMain 是在 loader lock 持有期间被调用的。在这个锁里做任何
        //   稍微复杂的事（加载 DLL、创建窗口、等待同步对象）都可能死锁 ——
        //   而且症状往往是"游戏直接卡在启动画面"，极其难查。
        //   标准做法：只 spawn 线程，立刻返回。
        std::thread::spawn(move || {
            if let Err(err) = run(module_raw as *mut c_void) {
                log::append(&format!("初始化失败: {err:?}"));
            }
        });
    }
    true   // 返回 true = 加载成功
}
```

## 2.4 单实例锁：防止"双重框"

```rust
/// 单实例占位：同一个进程里第二次注入会因为锁文件被独占打开而失败。
/// 返回的句柄必须一直活着（调用方持有），进程退出时自动失效。
fn claim_single_instance() -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .share_mode(0)          // ★ share_mode(0) = 独占打开，第二次会失败
        .open("C:\\csgo_hook.lock")
        .ok()
}

fn run(hmodule: *mut c_void) -> anyhow::Result<()> {
    // ★ 防双重框：如果同一个游戏进程里被注入了两份 DLL，
    //   两份都会挂钩 Present → 每个敌人画两遍框，看起来像重影。
    //   这里用锁文件挡住后注入的那一份（它不挂钩、直接返回）。
    let _instance_lock = match claim_single_instance() {
        Some(f) => f,
        None => {
            log::append("检测到已有另一个实例在运行 → 本实例不挂钩");
            return Ok(());
        }
    };
    // ...
}
```

---

## 2.5 构建与注入

### 构建

```bash
cargo build --release
# 产物：target/release/<你的包名>.dll
```

### 注入

思路就一句：**用任意能 LoadLibrary 的工具把 DLL 塞进游戏进程**。

| 方式 | 说明 |
|---|---|
| hudhook 仓库自带的 injector | 最省事，直接适配 hudhook 的初始化流程 |
| 通用注入器（Xenos / Extreme Injector） | 选 LoadLibrary 模式，把 DLL 路径填进去 |
| 自己写一个 | `OpenProcess` → `VirtualAllocEx` → `WriteProcessMemory`(写 DLL 路径) → `CreateRemoteThread(LoadLibraryA)` |

### ★ 一个必踩的坑：DLL 被占用，编译失败

```
error: failed to remove file `target\release\xcpd.dll`
Caused by: 拒绝访问。 (os error 5)
```

**原因**：上一份 DLL 正被游戏进程加载着，Windows 不允许覆盖已映射的文件。
你正在测试的时候改代码，一定会遇到这个。

**三种解法**：

1. 先退出游戏再编译（最干净，但来回切很烦）
2. **挪开旧文件再编译**（推荐）——Windows 允许对已映射的 DLL 做**重命名**：
   ```powershell
   Rename-Item target\release\xcpd.dll xcpd_prev.dll
   ```
   旧文件留着还能当回滚点
3. 构建到另一个目录：`cargo build --release --target-dir target_build`

### ★ 另一个坑：日志头显示的还是旧版本

遇到过最浪费时间的情况：**改了代码、编译成功、重新注入，但"没效果"**。
排查半天发现——**注入器指向的是另一个 DLL 文件**（项目里当时躺着 `xcpd.dll` 和 `xcpd_next.dll` 两份）。

**唯一可靠的判断方法**：把版本号当构建指纹写进日志第一行（见 [11.2 节](#112-版本指纹一个真实踩过的坑)），
注入后**第一件事就是看那个版本号**。不是新的，说明你注入的文件不对。

### 验证清单（每次注入后按这个顺序看）

1. **日志第一行的版本指纹** —— 确认加载的是刚构建的那份
2. **有没有崩溃** —— 闪退就查第 3 章的 API 闸门
3. **角色数量对不对** —— 看 `角色扫描(...)` 那几行日志
4. **框画出来没** —— 没框就翻第 13 章排查表

### 开发循环

```
改代码 → 改版本号 → cargo build --release
      → 挪开/退出旧 DLL → 重新注入
      → 看日志 → 回到第一步
```

**这个循环你大概会重复几百次**，所以务必把"看日志"变成本能：

- 日志放在**固定路径**（本项目是 `C:\csgo_hook.log`）
- 用一个能自动滚动、能按关键词过滤的编辑器，占半个屏幕盯着
- 关键判定处都要打日志，并且**日志要写清"为什么没做事"**（见 [11.1 节](#111-诊断日志是第一生产力)）

**这个开发循环的效率，直接决定你的项目能做多深。**

---

# 第 3 章 内存读写与崩溃防护

## 3.1 泛型读内存

```rust
/// 读远程内存，返回 Option<T>。
///
/// ★ 为什么返回 Option 而不是 Result？
///   逆向场景里"读不到"是常态而不是异常 —— 坐标可能在一帧里是垃圾值、
///   对象可能已经被回收、地址可能过期。用 Option 能让调用方一路 `?` 短路，
///   不用写一堆 match，代码可读性天差地别。
///
/// ★ 为什么用 ReadProcessMemory 读自己的进程？
///   ① 注入后我们就在目标进程内部，读自己 = 零权限成本；
///   ② 它有内置的页保护检查 —— 读非法地址返回错误码，
///      而不像裸指针解引用那样直接崩掉整个游戏。
pub fn read<T: Copy>(addr: usize) -> Option<T> {
    if addr < 0x1000 {          // 空指针/极小地址直接拒绝，省一次系统调用
        return None;
    }
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let sz = std::mem::size_of::<T>();
    let mut got = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            out.as_mut_ptr() as *mut c_void,
            sz,
            &mut got,
        )
    };
    // ★ 必须检查 got == sz：跨页边界的读会"部分成功"，
    //   只判断 ok 的话会拿到半截数据，后面算出来的坐标就是垃圾。
    if ok != 0 && got == sz {
        Some(unsafe { out.assume_init() })
    } else {
        None
    }
}
```

## 3.2 ★ 最重要的坑：加载时调 IL2CPP API 会闪退

**这一节如果没看懂，你的项目会一直随机崩溃。**

实测现象：在"出生岛 / 上飞机 / 跳伞"这些**加载和切场景**的时刻调用 IL2CPP API（`il2cpp_class_from_name` / `il2cpp_class_get_name`），会拿到**已经失效的元数据指针**，之后一解引用就访问违例闪退（实测崩在 `il2cpp_class_get_name_api` 偏移 0x8B 处）。

**为什么**：切场景时 Unity 会重建 IL2CPP 的元数据，这期间类指针是悬空的。

**解决方案：给 API 调用加一个"关卡"，用渲染时间戳判断游戏是否稳定。**

```rust
/// 渲染线程每帧调 `api_tick()` 打时间戳；**加载时 Present 不会跑**，
/// 时间戳自然变旧 → 所有 API 调用被拒绝（返回 None），
/// 等回到能渲染的帧再恢复。
static API_TICK: AtomicU64 = AtomicU64::new(0);

/// 渲染线程每帧调用：标记“现在游戏能正常渲染，元数据是稳定的”。
pub fn api_tick() {
    API_TICK.store(ticks_ms(), Ordering::Relaxed);
}

/// 现在能否安全调 IL2CPP API（超过 400ms 没渲染过 = 正在加载，拒绝）。
fn api_ok() -> bool {
    let t = API_TICK.load(Ordering::Relaxed);
    // t == 0 表示渲染循环还没跑起来，同样不能调
    t != 0 && ticks_ms().saturating_sub(t) < 400
}
```

然后**每一个** IL2CPP API 调用的入口都加这一句：

```rust
pub fn il2cpp_find_class(namespace: &str, name: &str) -> Option<usize> {
    // ★ 加载/切场景时一律不调 API（元数据可能正在重建）
    if !api_ok() {
        return None;
    }
    // ...
}
```

**这个设计的妙处**：它不需要你去 hook Unity 的加载函数、也不需要判断"当前在哪个场景"。它利用了**游戏加载时不会渲染**这个事实，用一个时间戳就完成了状态判断。

> **迁移提示**：这条定律对所有 Unity IL2CPP 游戏都适用。只要你发现"偶尔在特定时机崩溃"，先检查是不是在调用 IL2CPP API 时没做这个校验。

## 3.3 可读性检查

有时候你需要在**扫描一大片内存**之前先确认这块地址可读（比如从候选地址往回找对象头）：

```rust
pub fn is_readable(addr: usize, size: usize) -> bool {
    // VirtualQuery 问操作系统：这块内存现在的保护属性是什么
    let mut mbi = unsafe { std::mem::zeroed::<MEMORY_BASIC_INFORMATION>() };
    let r = unsafe { VirtualQuery(addr as *const c_void, &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
    if r == 0 {
        return false;
    }
    // ★ 必须排除这几种页：未提交 / 不可访问 / 守护页
    //   守护页(PAGE_GUARD)尤其阴 —— 碰一下就会触发一次异常，
    //   如果没装结构化异常处理，游戏当场就没了。
    !matches!(
        mbi.Protect,
        PAGE_NOACCESS | PAGE_GUARD
    ) && mbi.State == MEM_COMMIT
}
```

---

# 第 4 章 IL2CPP：别硬扫内存

## 4.1 IL2CPP 给了你什么

Unity 用 IL2CPP 把 C# 编译成 C++ 再编译成原生代码，但它**保留了完整的元数据**：类名、字段名、字段偏移、静态字段全都能查到。

这意味着：**你不需要自己去猜"这个 0x7F8 是什么字段"，你可以直接问游戏"BattleRoleLogic 类的坐标字段在哪"。**

要用的导出函数（全部从 `GameAssembly.dll` 按名字拿到）：

| 函数 | 作用 |
|---|---|
| `il2cpp_domain_get` | 拿到当前 AppDomain |
| `il2cpp_domain_get_assemblies` | 枚举所有程序集 |
| `il2cpp_assembly_get_image` | 程序集 → 映像 |
| `il2cpp_class_from_name` | （映像，命名空间，类名）→ Il2CppClass* |
| `il2cpp_class_get_name` | Il2CppClass* → 类名 |

## 4.2 按（命名空间, 类名）找类

```rust
// ★ 一套 typedef：IL2CPP 的导出函数是 C ABI，用 transmute 转成 Rust 函数指针调用。
//   参数类型用 *mut c_void 而不是具体类型 —— 我们不需要知道 Il2CppClass 的布局，
//   只需要把它当不透明指针传来传去。
type FnDomainGet = unsafe extern "system" fn() -> *mut c_void;
type FnDomainGetAssemblies =
    unsafe extern "system" fn(*mut c_void, *mut usize) -> *mut *mut c_void;
type FnAssemblyGetImage = unsafe extern "system" fn(*mut c_void) -> *mut c_void;
type FnClassFromName =
    unsafe extern "system" fn(*mut c_void, *const u8, *const u8) -> *mut c_void;

pub fn il2cpp_find_class(namespace: &str, name: &str) -> Option<usize> {
    if !api_ok() { return None; }                 // ← 第 3 章的崩溃防护，必须有

    let module = GameAssembly_module_base()? as *mut c_void;
    unsafe {
        // GetProcAddress 按名字取函数地址。IL2CPP 这些是标准导出，不需要特征码扫描。
        let domain_get: FnDomainGet = std::mem::transmute(
            GetProcAddress(module, c"il2cpp_domain_get".as_ptr() as *const u8)?);
        let get_asms: FnDomainGetAssemblies = std::mem::transmute(
            GetProcAddress(module, c"il2cpp_domain_get_assemblies".as_ptr() as *const u8)?);
        let asm_img: FnAssemblyGetImage = std::mem::transmute(
            GetProcAddress(module, c"il2cpp_assembly_get_image".as_ptr() as *const u8)?);
        let cls_from_name: FnClassFromName = std::mem::transmute(
            GetProcAddress(module, c"il2cpp_class_from_name".as_ptr() as *const u8)?);

        let domain = domain_get();
        if domain.is_null() { return None; }

        let mut count: usize = 0;
        let arr = get_asms(domain, &mut count);   // ★ arr 是程序集指针数组
        if arr.is_null() || count == 0 { return None; }

        // CString：IL2CPP 要的是 C 字符串，必须保证生命周期活到调用结束
        let cname = std::ffi::CString::new(name).ok()?;
        let cns = std::ffi::CString::new(namespace).ok()?;

        for i in 0..count {
            // ★ 用 read::<usize> 读数组元素，而不是 `*arr.add(i)` 裸解引用。
            //   为什么？程序集数组在切场景时可能被回收/重建，
            //   裸解引用会直接崩；用 ReadProcessMemory 最多返回 None，安全跳过。
            let Some(asm) = read::<usize>((arr as usize) + i * 8) else {
                continue;
            };
            if asm == 0 { continue; }
            let img = asm_img(asm as *mut c_void);
            if img.is_null() { continue; }
            let klass = cls_from_name(img, cns.as_ptr() as *const u8, cname.as_ptr() as *const u8);
            if !klass.is_null() {
                return Some(klass as usize);
            }
        }
        None
    }
}
```

## 4.3 反过来：从对象拿类名（用于过滤垃圾数据）

扫描内存时你会捞到一堆**根本不是对象**的指针。判断"这东西是不是合法 IL2CPP 对象"最可靠的方法是——**它的第一个字段指向的类，类名对不对**：

```rust
/// 传入对象指针，返回它的类名。
pub fn il2cpp_class_get_name_api(klass: usize) -> Option<String> {
    if !api_ok() { return None; }                 // ← 同样必须
    let module = GameAssembly_module_base()?;
    unsafe {
        let f: unsafe extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(
            GetProcAddress(module as *mut c_void, c"il2cpp_class_get_name".as_ptr() as *const u8)?);
        let p = f(klass as *mut c_void);
        if p.is_null() { return None; }
        // 用 CStr 而不是 read_c_string：返回的字符串是游戏自己的静态数据，
        // 生命周期由游戏管理，不需要我们复制。
        Some(std::ffi::CStr::from_ptr(p as *const i8).to_string_lossy().into_owned())
    }
}
```

**用法**（这是本项目最关键的过滤器）：

```rust
for &o in raw_candidates {
    // 每个 IL2CPP 对象的第 0 个字段就是它的 Il2CppClass*
    let klass = memory::read::<usize>(o);
    let name = klass
        .and_then(memory::il2cpp_class_get_name_api)
        .unwrap_or_else(|| "?".into());

    // ★ 只要类名 == "BattleRoleLogic"，这货就是我们要的角色对象。
    //   这一招能过滤掉 99% 的垃圾：别的类的对象、链表节点、结构体内部指针，
    //   类名一定不是它。
    if name == ROLE_CLASS_NAME {
        roles.push(o);
    }
}
```

> **为什么这招这么好用**：因为**类指针是内存里最难伪造的东西**。你随便扫一个地址，它的第 0 个 qword 指向的地址，要能通过 `il2cpp_class_get_name` 返回一个合理的字符串——这个概率低到可以忽略。反过来，如果只按"位置字段看起来像坐标"来过滤，会捞到一大堆假货（本项目实测：全内存结构扫描捞出几百个"候选"，其中真对象只有十几个）。

---

# 第 5 章 找到角色列表：三条路

这是整个项目的第一个难关。你要拿到"所有玩家对象的指针数组"。

## 5.1 三条路对比

| 路线 | 原理 | 优点 | 缺点 | 本项目结论 |
|---|---|---|---|---|
| ① 静态链 | `GameWorldClientManager → BattleWorld → startGame → StartGame.RoleList` | 快（几个解引用） | 依赖一串绝对地址，游戏一更新就全断 | 作**主路**，但要配合类名校验 |
| ② 按类名枚举 | 遍历场景对象，过滤类名 == `BattleRoleLogic` | 抗更新（类名很少变） | 需要遍历，稍微慢 | **主力** |
| ③ 全内存扫描 | 暴力扫内存找"连续指向同类对象的数组" | 任何情况都能找到 | 秒级耗时，CPU 打满，**千万别无条件跑** | 只做手动兜底 |

## 5.2 路线③的教训：一定要有闸门

项目里有一段血泪注释：

```rust
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
```

**通用教训**：任何"兜底逻辑"都必须有开关和限频。否则它会在你没注意的时候把游戏卡死，而你还以为是别的原因。

## 5.3 路线②的实现核心：类名 + 坐标双重校验

```rust
fn filter_roles(raw: &[usize]) -> (Vec<usize>, bool, String) {
    let mut names: HashMap<String, usize> = HashMap::new();   // 用于诊断日志
    let mut v: Vec<usize> = Vec::new();                        // 全部合法坐标对象
    let mut v_match: Vec<usize> = Vec::new();                  // 类名匹配的

    for &o in raw {
        let name = match memory::read::<usize>(o) {
            Some(k) => memory::il2cpp_class_get_name_api(k).unwrap_or_else(|| "?".into()),
            None => "?".into(),
        };
        *names.entry(name.clone()).or_insert(0) += 1;

        // 校验 ①：坐标必须能读出来且是有限数（NaN/Inf 说明这不是活对象）
        let Some(p) = read_role_pos(o + ROLE_POS_OFF) else { continue };
        if !(p.x.is_finite() && p.y.is_finite() && p.z.is_finite()) {
            continue;
        }

        // 校验 ②：地图坐标范围。
        // ★ 这里从 5000 放宽到了 20000 —— 因为出生岛在 ±5000 附近，
        //   原来的 5000 阈值会把出生岛上的自己当成"无效坐标"过滤掉
        //   （日志里出现 `采用=0` 就是这个原因，排查了半天）。
        if p.x.abs() > 20000.0 || p.z.abs() > 20000.0 {
            continue;
        }

        if name == ROLE_CLASS_NAME {
            v_match.push(o);
        }
        v.push(o);
    }

    // ★ 优先返回类名匹配的那批；一个都没有才退回"坐标合法"的全部。
    //   为什么留这个退路？有些游戏会把类名混淆，或者类名带命名空间，
    //   这时类名匹配会全军覆没 —— 至少还有坐标过滤的结果可用。
    let matched = !v_match.is_empty();
    let v = if matched { v_match } else { v };
    // ...
    (v, matched, dist_s)
}
```

**同时把类名分布写进日志**：

```
角色扫描(权威链 StartGame.RoleList):
  list@0x50: 16 个
  合并后共 16 个角色
  原始=16 采用=16 (类名匹配=true) 类名分布: BattleRoleLogic×16
```

**为什么要打这个日志**：当扫描出问题的时候（比如"敌人一个都没有"），你一眼就能看出是"类名变了"（分布里全是别的类名）还是"列表本身是空的"（原始=0）。**这是排查效率的关键**。

---

# 第 6 章 挖偏移：把 dump 当线索，不当真相

## 6.1 先拿到 dump

用 **Il2CppDumper**（开源工具）配合游戏的 `GameAssembly.dll` + `global-metadata.dat`，能得到一份 `dump.cs`，里面有所有类、字段名、字段偏移。

**但请注意**：dump 里的字段名经常是混淆的（`$A`、`$b`、`$cC`），而且**字段偏移和实际内存布局可能不一致**（尤其是编译器优化/继承/内联之后）。

## 6.2 铁律：dump 说是什么不算，日志说是什么才算

本项目的真实例子：

```
dump 里：  private Vector3 $A;   // 0xA4
实测：     读 0xA4 → 永远是常量 (0,1,0)   ← 不是坐标！

dump 里：  private Vector3 $t;   // 0x7F8
实测：     读 0x7F8 → 正常的、随移动变化的地图坐标  ← 这才是
```

所以 offset.rs 里写着：

```rust
/// 角色坐标（Vector3）。
/// ★ 实证：是 `private Vector3 $t; // 0x7F8`（连排布局），
///   而**不是** dump 里那个显眼的 `$A // 0xA4` —— 那个读出来是常量 (0,1,0)。
pub const ROLE_POS_OFF: usize = 0x7F8;
```

## 6.3 挖偏移的标准流程（可复用）

整套方法就一句话：**制造一个可观测的变化，然后扫描"哪个地址跟着变了"。**

### 方法 A：蹲下差异法（找姿态字段）

```
① 站着不动，把角色的所有候选字段值 dump 出来（只读，不影响游戏）
② 蹲下
③ 再 dump 一次
④ 对比：只有姿态字段会变（实测：站立 ≈0.65 → 蹲下 ≈0.30）
```

实测输出（这就是找到 `STANCE_OFF = 0x264` 的过程）：

```
A(站)  0x264 = 0.650    0x218 = 0(枚举)
B(蹲)  0x264 = 0.300    0x218 = 2(枚举)
```

**找到了两个东西**：
- `0x264` 是**比例值**（0.65/0.30），适合用来按比例压矮 ESP 框
- `0x218` 是**枚举**（0=站/2=蹲/3=趴），适合用来做分支判断

### 方法 B：血量差异法（找血量字段）

```
① 满血 → dump 所有 float == 100 的候选地址
② 掉一点血 → dump 所有 float == 87 的候选地址
③ 取交集 → 两边都命中的那个就是血量
```

代码骨架：

```rust
// 第一步：收集所有"值等于 100"的地址
let cands_a = memory::scan_f32(100.0, 0.5, 20000);   // (目标值, 容差, 最多返回几个)
// 第二步：掉血后再收集"值等于 87"的
let cands_b = memory::scan_f32(87.0, 0.5, 20000);
// 第三步：交集
for a in cands_a { if cands_b.contains(&a) { println!("血量地址 = {a:X}"); } }
```

### 方法 C：头脚差异法（找真实头部坐标）

```
① 读角色坐标（脚底）
② 同时读候选的头部路径
③ 计算 dY = 头.y - 脚.y
④ 正常应该 ≈ 1.2~1.3 米（人物身高）
⑤ 然后蹲下，看哪个候选跟着脑袋一起沉下去
```

实测结论（这是一个**必须用日志验证**的典型）：

```
实测 2026-10-04 已验证：
  BattleRoleLogic +0xAF0 → RoleLogicClient
  RoleLogicClient +0x80  → BattleRole（RoleClient）
  BattleRole      +0x240 → RoleControl
  RoleControl     +0x16C → Vector3 headPoint     ← 游戏自己的 GetHeadPoint()
  4 个角色 dY 全是 1.26~1.27、水平差 0.00；自己蹲下 y 从 80.49 掉到 79.98
```

**注意最后那半句**："自己蹲下 y 从 80.49 掉到 79.98"——这证明**这个头是真头，会跟着蹲下动**。如果没有这个验证，你可能是找错了字段（读到一个固定不动的东西）。

## 6.4 挖偏移的工程化：写成一次性脚本

**不要**在正式代码里写一堆临时打印。做法是：

1. 写一个**专门的诊断函数**（比如 `scan_bones_to_log()`）
2. 从菜单按钮调用它
3. 结果写日志文件（不是 stdout —— 注入的 DLL 没有控制台）
4. 挖到之后，**把这个函数删掉**（或者留作历史参考）

```rust
/// 诊断函数：扫描并写日志。挖到偏移后就不需要了。
fn scan_bones_to_log() {
    let mut out = String::new();
    out.push_str("\n=== 骨骼扫描 ===\n");
    for (path, off) in candidate_paths {
        let v = memory::read_vec3_contig(base + off);
        out.push_str(&format!("  {path} +0x{off:X} → {v:?}\n"));
    }
    log::append(&out);          // ← 写文件，不是 println!
}
```

## 6.5 日志系统也要限流

**反面教材**（本项目早期）：内存读取线程每 2ms 跑一轮，直接写文件会**刷爆磁盘**（几百 MB 的日志文件）。

```rust
/// 状态变化时追加一行；与上一条相同、或距上一条太近，则忽略。
pub fn stage(stage: &'static str) {
    let now = now_ms();
    let prev = LAST_WRITE_MS.load(Ordering::Relaxed);

    // ① 全局最小间隔：防止状态来回跳变时刷屏
    if prev != 0 && now.saturating_sub(prev) < MIN_INTERVAL_MS {   // MIN_INTERVAL_MS = 1500
        return;
    }
    // ② 内容没变也不写
    let mut last = guard();
    if *last == Some(stage) {
        return;
    }
    // ...
}
```

**两层过滤**：内容相同不写、间隔太近不写。这样日志既不会刷爆，又能保留状态变化的历史。

---

# 第 7 章 世界坐标 → 屏幕

## 7.1 视图投影矩阵是什么

游戏渲染时会把每个物体做两步变换：

```
世界坐标 --(视图矩阵)--> 相机坐标 --(投影矩阵)--> 裁剪空间 --(齐次除法)--> NDC(-1..1) --(视口变换)--> 屏幕像素
```

游戏通常把这两步合并成一个 **View-Projection 矩阵**存在内存里。你只要找到它、跟坐标相乘，就能算出"这个敌人在屏幕哪个像素"。

## 7.2 找到矩阵：链式 + 校验

```rust
/// UnityPlayer.dll 里的静态链：
/// VIEW_MATRIX → +0x0 → +0x40 → +0x100 = [f32; 16]
pub const VIEW_MATRIX: usize = 0x01C1ACC0;
pub const VIEW_MATRIX_OFFSET_1: usize = 0x0;
pub const VIEW_MATRIX_OFFSET_2: usize = 0x40;
pub const VIEW_MATRIX_OFFSET_3: usize = 0x100;
```

**但光拿地址不够**，因为：
- 链上某一跳可能读到空/垃圾
- 加载时矩阵是单位阵
- 可能会读到**别的相机**的矩阵（比如小地图相机、UI 相机）

所以必须**校验**。核心思路：**从矩阵反推相机位置，看它是否落在玩家附近**。

```rust
/// 从视图矩阵反推出相机世界坐标。
///
/// ★ 数学原理：视图矩阵的平移列 = -(相机位置 · 基向量)，
///   所以用 m[12],m[13],m[14] 除以各基向量的模长就能反解出相机位置。
///   这个反推出来的相机位置，是我们判断"矩阵对不对"的关键依据。
pub fn camera_position(matrix: [f32; 16]) -> Option<Vec3> {
    let r = [m[0], m[4], m[8]];      // 右基向量
    let u = [m[1], m[5], m[9]];      // 上基向量
    let f = [m[2], m[6], m[10]];     // 前基向量
    let a = norm3(r); let b = norm3(u); let c = norm3(f);
    if a < 1e-6 || b < 1e-6 || c < 1e-6 { return None; }

    let dr = -m[12] / a;
    let du = -m[13] / b;
    let df = -m[15] / c;
    Some(Vec3 {
        x: dr * r[0] + du * u[0] + df * f[0],
        y: dr * r[1] + du * u[1] + df * f[1],
        z: dr * r[2] + du * u[2] + df * f[2],
    })
}
```

```rust
/// 这个矩阵像不像一个正常的视图投影矩阵？
fn matrix_ok(m: &[f32; 16], refs: &[Vec3], max_d: f32) -> bool {
    // ① 全部有限且不过大（矩阵坏掉时经常算出 1e30，会把 ImGui 冲爆导致崩溃）
    if m.iter().any(|v| !v.is_finite() || v.abs() > 1.0e7) {
        return false;
    }

    // ② w 行模长：正常 VP 矩阵 ≈ 1
    let w = (m[3] * m[3] + m[7] * m[7] + m[11] * m[11]).sqrt();
    if !(0.5..=1.8).contains(&w) {
        return false;
    }

    // ③ 必须带平移 —— 这一条专门用来拒掉单位阵
    //    （加载时链上读到的常常是全零/单位阵，过不了这一关）
    if m[12].abs() + m[13].abs() + m[14].abs() < 1.0 {
        return false;
    }

    // ④ ★ 最有力的一条：反推出的相机位置必须离玩家不太远。
    //    第三人称游戏相机一般在自己身后 4~8 米；
    //    小地图相机/UI 相机的位置就完全对不上了。
    let Some(c) = memory::camera_position(*m) else { return false };
    if refs.is_empty() { return true; }
    cam_plausible(c, refs, max_d)
}

/// 相机是否在"任何一个参考点"附近
fn cam_plausible(c: Vec3, refs: &[Vec3], max_d: f32) -> bool {
    refs.iter().any(|p| {
        let dx = c.x - p.x;
        let dz = c.z - p.z;
        (dx * dx + dz * dz).sqrt() < max_d
    })
}
```

**取矩阵的完整策略（两档阈值）**：

```rust
/// 取视图矩阵。顺序很关键：
///   ① 严格：相机水平方向就在玩家附近（第三人称 4~8 米）—— 几乎不可能误判；
///   ② 宽松：滑翔/滞空时相机可能离自己几十米。
fn resolve_view_matrix(module: usize, refs: &[Vec3]) -> Option<[f32; 16]> {
    let cands = memory::chain_view_matrix_candidates(module);

    // 先用严标准过一遍所有候选
    for m in cands.iter() {
        if matrix_ok(m, refs, 30.0) {
            return Some(*m);
        }
    }
    // 严标准全灭 → 放宽到 200 米（滑翔/跳伞时的情形）
    for m in cands.iter() {
        if matrix_ok(m, refs, 200.0) {
            return Some(*m);
        }
    }
    // ...
}
```

## 7.3 乘法：注意行主序/列主序

**这是最容易出错的地方。** 同一个矩阵，按"行主序"和"列主序"解释会得到完全不同的结果。

```rust
/// 世界坐标 -> NDC。返回 (ndc_x, ndc_y, w)。
///
/// - 列主序(Unity/D3D 常见)：每一项取“列”的前三个元素 (m0,m4,m8 / m1,m5,m9)
/// - 行主序(转置)：每一项取“行”的前三个元素 (m0,m1,m2 / m4,m5,m6)
pub fn project_ndc(world: Vec3, matrix: [f32; 16], opt: Projection) -> Option<(f32, f32, f32)> {
    let m = matrix;

    let (x, y, w) = if opt.row_major {
        // 行主序：第 i 行的元素是连续的 m[i*4..i*4+3]
        (
            world.x * m[0] + world.y * m[1] + world.z * m[2]  + m[3],
            world.x * m[4] + world.y * m[5] + world.z * m[6]  + m[7],
            world.x * m[12] + world.y * m[13] + world.z * m[14] + m[15],
        )
    } else {
        // 列主序：第 i 行的元素是跨步的 m[i], m[i+4], m[i+8]
        (
            world.x * m[0] + world.y * m[4] + world.z * m[8]  + m[12],
            world.x * m[1] + world.y * m[5] + world.z * m[9]  + m[13],
            world.x * m[3] + world.y * m[7] + world.z * m[11] + m[15],
        )
    };

    // ★★ 这里有个真实踩过的坑，原文注释保留：
    // 原来写的是 `if w < 0.001 && !w.is_finite()` —— 两个条件几乎不可能同时成立，
    // 等于没过滤！相机背后(w<=0)的点会算出巨大坐标，然后 ImGui 画一个
    // 坐标几十万的矩形 → D3D 直接崩。
    // 正确写法是 !(w > 0.001)，同时覆盖 w<=0、w 极小、NaN 三种情况。
    if !(w > 0.001) {
        return None;
    }

    let ndc_x = x / w;
    let ndc_y = y / w;
    if !ndc_x.is_finite() || !ndc_y.is_finite() {
        return None;                 // 再兜一层：除法可能溢出成 Inf
    }

    Some((ndc_x, ndc_y, w))
}
```

**经验法则**：如果你发现"框的位置左右反了/上下反了/完全乱飞"，挨个试这四个组合（行/列主序 × 是否翻转）。

## 7.4 NDC → 屏幕像素

```rust
pub fn world_to_screen(
    world: Vec3, matrix: [f32; 16],
    width: f32, height: f32, opt: Projection,
) -> Option<Vec2> {
    let (ndc_x, ndc_y, _w) = project_ndc(world, matrix, opt)?;

    let center_x = width * 0.5;
    let center_y = height * 0.5;

    // NDC 是 -1..1，映射到 0..width / 0..height
    let sx = if opt.flip_x { center_x - ndc_x * center_x } else { center_x + ndc_x * center_x };

    // ★ 注意 Y 是反的：NDC 里 y=+1 是屏幕上方，而屏幕坐标 y 向下增长。
    //   所以默认是 `center_y - ndc_y * center_y`（减号）。
    let sy = if opt.flip_y { center_y + ndc_y * center_y } else { center_y - ndc_y * center_y };

    Some(Vec2::new(sx, sy))
}
```

---

# 第 8 章 ESP 绘制

## 8.1 框怎么画

拿到屏幕坐标后，画框需要的是**左上角 + 右下角**。有两种算法：

| 算法 | 做法 | 问题 |
|---|---|---|
| 世界四角投影 | 把人物包围盒的 8 个角投影到屏幕，取 AABB | **近距离时会变得极宽**（透视畸变），框会飘到屏幕外 |
| **比例框**（本项目选的） | 投影头脚 → 得到高度，宽度 = 高度 × 人物宽高比 | 稳定，任何距离都贴合 |

```rust
    // 框：用「投影高度 × 宽高比」算 —— 稳定、不会被透视/贴近相机拉宽，
    // x 取头脚中点，所以始终贴在身体上（自瞄瞄的也是这个头）。
    let height_px = (feet.y - head.y).abs().max(1.0);          // 头脚屏幕距离 = 框高

    // ★ ratio 的算法：世界里的"宽/高"比例，映射到屏幕上的像素比例。
    //   box_width 是人物肩宽（米），player_height 是身高（米），
    //   两者相除就是宽高比，再 clamp 防止极端值。
    let ratio = (config.box_width / config.player_height.max(0.2)).clamp(0.15, 0.8);

    let (box_min, box_max) = ratio_box(head, feet, height_px, ratio, config.box_pad);
```

```rust
/// 比例框：x 取头脚中点，宽度 = 高度 × ratio
fn ratio_box(
    head: memory::Vec2,
    feet: memory::Vec2,
    height_px: f32,
    ratio: f32,
    pad: f32,
) -> ([f32; 2], [f32; 2]) {
    // ★ .max(8.0)：远距离时框高可能只有一两个像素，宽度会退化成 0，
    //   画出来是"一条线"或者干脆看不见。给个下限保证永远能看到一个框。
    let w = (height_px * ratio).max(8.0);

    let cx = (head.x + feet.x) * 0.5;        // 水平中心：头脚中点

    (
        // ★ 用 min/max 而不是"head 在上、feet 在下"的死假设：
        //   目标从头顶往下看（比如你在高处）时，屏幕上的 head.y 可能大于 feet.y，
        //   死假设会导致框反着画。用 min/max 一次解决上下颠倒的所有情况。
        [cx - w * 0.5 - pad, head.y.min(feet.y) - pad],   // 左上
        [cx + w * 0.5 + pad, head.y.max(feet.y) + pad],   // 右下
    )
}
```

> **这段代码里有两处细节值得单独记下来**——`max(8.0)` 和 `min/max` 都来自实际踩坑：
> 前者是"远距离看不到框"，后者是"在高处俯视时框画反了"。
> 这类细节是"能跑"和"好用"之间的差距。

## 8.2 防御：坐标非法就不画

```rust
    // ★ 防御：坐标非法/过大（矩阵坏时可能算出 1e30）→ 直接不画。
    //   不画最多是"这一帧没框"，画了可能让 D3D 崩掉整个游戏。
    let sane = |v: f32| v.is_finite() && v.abs() < 100_000.0;
    if !(sane(box_min[0]) && sane(box_min[1]) && sane(box_max[0]) && sane(box_max[1])) {
        return;
    }
```

## 8.3 姿态跟随：蹲下时框要变矮

**问题**：敌人蹲下时头变低了，如果你的框高用的是固定值，框就会"悬在人头上方"。

**解决**：用第 6 章挖出来的**姿态比例字段**动态压矮框：

```rust
/// 姿态枚举 → 身高比例（"没有真头"时用它估框顶高度，让框跟着蹲下/趴下变矮）。
/// AC_StandState: Stand=1 / Crouch=2 / Prone=3 / Fly=4
pub fn stance_height_scale(stance_enum: i32) -> f32 {
    match stance_enum {
        2 => 0.65,     // 蹲下
        3 => 0.30,     // 趴下
        _ => 1.0,
    }
}
```

## 8.4 位置平滑：治"框跟随一卡一卡"

**现象**：人机（AI）的坐标是**服务端低频同步**的，每 100~200ms 才更新一次，导致框会"一跳一跳"。

**解决**：一阶低通滤波（指数平滑）。

```rust
/// 位置平滑（治"框跟随一卡一卡"）。
///
/// ★ 关键设计：即使坐标是阶梯式跳变的，我们也要让 vel（速度）平滑 ——
///   因为自瞄的"超前补偿"要靠这个速度算提前量。如果速度直接用差分，
///   会得到一堆脉冲噪声。
fn smooth_pos(role: usize, feet: Vec3, head: Option<Vec3>, now: u64)
    -> (Vec3, Option<Vec3>, Vec3)
{
    // 每个角色单独维护一份平滑状态（按指针做 key）
    // ...
    let dt = ((now.saturating_sub(e.last_ms)) as f32 / 1000.0).clamp(0.001, 0.2);

    // ★ 一阶低通：a 越小平滑越强、滞后越大。
    //   用 `1 - exp(-dt/tau)` 而不是固定系数 —— 这样即使帧率变化
    //   平滑程度也保持一致（固定系数在掉帧时会变成"几乎不平滑"）。
    let a = 1.0 - (-dt / tau).exp();

    let nx = e.feet.x + (feet.x - e.feet.x) * a;
    // ...
}
```

## 8.5 ESP 的绘制清单

| 元素 | 数据来源 | 备注 |
|---|---|---|
| 方框 | 头脚投影 | 见 8.1 |
| 血条 | `RoleNetMirror.SyncHp` | 人机常常读不到（服务端不下发），要容错 |
| 昵称 | `BattleRoleLogic.NickName` | 人机的名字可能在 `RoleAILogic.Name`，要准备两条路 |
| 骨架 | 头 + 脚两点 | 别费劲找全套骨骼，两点画的"棍人"已经够用 |
| 敌我染色 | 队伍号比较 | ★ 注意队伍号 0 是合法值 |
| 射线 | 屏幕底边中点 → 头顶 | 简单直线 |

## 8.6 敌我判断的一个大坑

```rust
// ★★ 血泪教训：队伍号 0 是合法的！
// 一开始写成了 `if team > 0 { ... }` 来判断"有队伍信息"，
// 结果 0 队的所有人被当成"没有队伍信息"处理 → 敌我染色全乱。
// 正确做法：用 -1 表示"读不到"，0 照常参与比较。
pub const ROLE_TEAM_OFF: usize = 0x7B0;    // 实测取值 0/1/2

// 读不到时返回 -1（而不是 0），这样调用方才能区分"0 队"和"没读到"
fn read_role_team(role: usize) -> i64 {
    if let Some(v) = memory::read::<i64>(role + ROLE_TEAM_OFF) { return v; }
    // ... 其他备选路径
    -1
}
```

---

# 第 9 章 自瞄

## 9.1 为什么不用"写内存改视角"

很多教程会教你：直接把角色的相机角度写进内存，瞬间锁头。

**风险极高**：
- 瞬间对齐视角是**教科书式的异常行为**，任何行为分析反作弊一眼就能看出来
- 这个游戏里明确有 `AnalyzeAimbot` 之类的检测类

**所以本项目选的是"模拟人类鼠标移动"**：算出目标在屏幕上的位置 → 计算需要移动多少像素 → 用 `SendInput` 发鼠标位移。行为上和真人鼠标一致。

```rust
/// 发送鼠标相对位移。
///
/// ★ 用 SendInput 而不是 SetCursorPos：
///   SetCursorPos 是"瞬移"，会让视角瞬间跳过去 —— 同样很容易被检测。
///   MOUSEEVENTF_MOVE 是相对位移，游戏收到的是"鼠标移动了 dx,dy"，
///   和真实鼠标产生的消息完全一样。
fn move_mouse(dx: i32, dy: i32) {
    if dx == 0 && dy == 0 { return; }
    let inp = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx, dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe { SendInput(1, &inp, std::mem::size_of::<INPUT>() as i32); }
}
```

## 9.2 自瞄的完整流程

```
每帧（在游戏的渲染帧里）：
  ① 检查：开关开了吗？菜单关了吗？按住触发键了吗？
  ② 遍历所有目标 → 算世界坐标 → 投影到屏幕 → 在 FOV 圈内的才要
  ③ 选目标：优先沿用上一帧锁定的目标（粘滞），否则选离准心最近的
  ④ 算误差：ex = 目标屏幕x - 准心x
  ⑤ 算鼠标位移：mx = ex × 增益
  ⑥ SendInput 发出去
```

## 9.3 关键难点：你不知道玩家的游戏内灵敏度

**这是自瞄最核心的工程问题**：屏幕上是 100 像素的误差，鼠标该移动多少 counts？

不同玩家的灵敏度设置差 10 倍，写死一个系数必然要么"太慢追不上"要么"冲过去来回摆"。

**解决方案：自适应学习。**

```rust
/// 每像素需要多少鼠标计数（×1000 存整数，避免原子变量用 f32）。
static KX: AtomicI32 = AtomicI32::new(300);   // 初始猜测 0.3
static KY: AtomicI32 = AtomicI32::new(300);
/// 上一帧的误差（像素×100）和实际发出的位移（counts），用来反推 K
static PREV_EX: AtomicI32 = AtomicI32::new(0);
static PREV_MX: AtomicI32 = AtomicI32::new(0);
```

```rust
    let mut kx = KX.load(Ordering::Relaxed) as f32 / 1000.0;

    if HAVE_PREV.load(Ordering::Relaxed) {
        let pmx = PREV_MX.load(Ordering::Relaxed) as f32;      // 上次发了多少 counts
        let pex = PREV_EX.load(Ordering::Relaxed) as f32 / 100.0;
        let dxs = pex - ex;                                     // 误差减少了多少像素

        // ★ 三个条件缺一不可：
        //   ① 上次确实发了足够大的位移（>=2 counts，太小的话测量噪声占主导）
        //   ② 误差确实明显减小了（>2 px）
        //   ③ 位移和误差减少**同号**（否则说明这帧目标跳了，数据不可用）
        if pmx.abs() >= 2.0 && dxs.abs() > 2.0 && pmx * dxs > 0.0 {
            // 反推：如果发了 pmx 个 count，误差减少了 dxs 像素，
            //       那么"每像素需要多少 count" ≈ pmx / dxs
            let est = (pmx / dxs).clamp(0.05, 1.5);

            // ★ EMA 平滑更新，学习率只有 0.12 —— 学得慢但不会震荡。
            //   早期版本用 0.3 的学习率 + 宽得多的 clamp 范围(0.02~20)，
            //   结果 K 自己在震荡，准心跟着抖，非常明显。
            kx = (kx * 0.88 + est * 0.12).clamp(0.05, 1.5);
            KX.store((kx * 1000.0) as i32, Ordering::Relaxed);
        }
    }
```

**这个设计的精妙之处**：它把"玩家灵敏度是多少"这个未知量，转化成了一个**可在线估计的参数**。你完全不需要知道游戏设置，它会自己收敛。

## 9.4 防抖三件套

就算 K 算对了，还有三个抖动来源，必须逐个治：

```rust
/// 死区（像素）：已经贴住准心就别再动鼠标 —— 治抖动最关键的一条。
const DEAD_ZONE_PX: f32 = 4.0;

/// 单帧最大鼠标位移（counts）—— 防止大误差时一帧冲过去
const MAX_STEP: f32 = 36.0;
```

```rust
    // ① 死区：目标离准心 < 4 像素 → 完全不动鼠标
    if dist < DEAD_ZONE_PX {
        HAVE_PREV.store(false, Ordering::Relaxed);
        return;                    // ★ 注意这里要重置学习状态，否则下次会用过期数据
    }

    // ② 只走剩余误差的一部分（frac < 1 → 只会收敛不会振荡）
    //    并且封顶 0.5，保证任何灵敏度下都不会"冲过头 → 反向 → 再来"地摆
    let frac = (0.55 / config.aim_smooth.max(1.0)).min(0.5);
    let speed = config.aim_speed.max(0.1);
    let mx = (ex * frac * kx * speed).clamp(-MAX_STEP, MAX_STEP).round() as i32;
    let my = (ey * frac * ky * speed).clamp(-MAX_STEP, MAX_STEP).round() as i32;

    // ③ 取整后为 0 就不发（避免发一堆无意义的 0 位移）
    if mx == 0 && my == 0 {
        return;
    }
```

**为什么用"只走一部分误差"而不是"一次走完"**：这是控制论里的经典做法。走 100% 误差时，任何过冲都会变成反向振荡；走 30~50% 则单调收敛，代价是慢一点，但对自瞄来说"稳"比"快"重要得多。

## 9.5 目标粘滞：防止准心左右横跳

**问题**：两个人一前一后站着，你每帧都选"离准心最近的那个"，准心就会在两个目标之间来回跳。

**解决**：优先沿用上一帧的目标，只有当它明显变得更远时才换。

```rust
    // ② 上一帧锁定的目标还在视野里的话：只有当它离准心不比"最近的那个"远超过
    //    aim_stick_px 像素时才继续用它（= 不横跳）；阈值设 0 就是"永远瞄离准心最近的"。
    let mut sticky_cand: Option<(f32, memory::Vec2, usize)> = None;
    let sticky = STICKY.load(Ordering::Relaxed);
    if sticky != 0 {
        if let Some(p) = snapshot.players.iter().find(|p| p.ptr == sticky) {
            if usable(config, snapshot, p) {
                if let Some(s) = screen_of(p) {
                    if let Some(d) = in_fov(s) {
                        sticky_cand = Some((d, s, p.ptr));
                    }
                }
            }
        }
    }
    let keep_sticky = match (sticky_cand.as_ref(), best.as_ref()) {
        // 锁定目标的距离 ≤ 最近目标的距离 + 阈值 → 继续用锁定的
        (Some(sd), Some(bd)) => sd.0 <= bd.0 + config.aim_stick_px,
        (Some(_), None) => true,       // 只有它在 FOV 内 → 当然用它
        _ => false,
    };
```

## 9.6 超前补偿：治"自瞄总是落在移动敌人身后"

**问题根源**：你的自瞄是**闭环控制**——看到误差才动鼠标。而"平滑 + 只走一部分误差"会引入**稳态滞后**：目标匀速移动时，准心会稳定地落后一段距离。

**解决**：估计目标在屏幕上的移动速度，然后**瞄它未来的位置**。

```rust
/// 返回"超前量"（像素）：目标速度 × 补偿帧数。换目标自动归零。
fn screen_lead(ptr: usize, x: f32, y: f32, lead_frames: f32) -> (f32, f32) {
    // ...
    let dx = (x - px).clamp(-cap, cap);      // 限制单帧跳变（目标切换时会突变）
    let dy = (y - py).clamp(-cap, cap);

    // 速度也用 EMA 平滑，否则噪声会让补偿量抖动
    vx = f32::from_bits(TGT_VX.load(Ordering::Relaxed)) * 0.6 + dx * 0.4;
    // ...
    (vx * l, vy * l)     // l = 补偿帧数（菜单可调）
}
```

---

# 第 10 章 边界情况：探头、人机、幽灵框

**这一章是"能不能真正好用"的分水岭。** 基础的 ESP + 自瞄可能一天就写出来了，但剩下这些细节会占掉你 70% 的时间。

## 10.1 探头（侧身）打不到头

**现象**：敌人站着能打中头，一探头（侧身）就打不到。

**原因**：人物的 `headPoint` 给的是**身体中轴线上**的头。探头时模型把头往侧面探出去约 0.5 米，但 `headPoint` 不会跟着横向移动。

**解决**：用「探头状态 × 朝向」把瞄点横向平移。

```rust
/// 头部水平补偿（世界坐标 XZ 增量）。
/// Unity: forward=(sin yaw, 0, cos yaw)，right=(cos yaw, 0, -sin yaw)
/// - 趴下：头在身体**前方**约 prone_fwd_m（headPoint 只给高度、不给前移）
/// - 底盘：头往前一点
/// - 探头/侧身：头往右/左平移（SyncSideAim = ±1）
pub fn head_shift(config: &Config, p: &Player) -> (f32, f32) {
    // ★ 滑翔/飞行状态不做任何水平补偿：此时 headPoint 挂在滑翔翼上，
    //   补偿会把框/瞄点推到身体前面去（用户反馈过"滑翔时框跑到敌人特别前面"）
    if p.stance_enum == 4 {
        return (0.0, 0.0);
    }

    let (sy, cy) = p.yaw.sin_cos();

    // 前后方向的前移量（趴下/蹲下时头向前）
    let fwd = match p.stance_enum {
        3 => config.prone_fwd_m,
        2 => config.crouch_fwd_m,
        _ => 0.0,
    };

    // 左右方向的探头量
    let mut lat = 0.0;
    if config.lean_follow {
        let mut s = p.lean_smooth.clamp(-1.0, 1.0);
        if config.lean_invert { s = -s; }
        lat = config.lean_shift_m * s;
    }

    // ★ 把"前移"和"侧移"两个分量按朝向旋转到世界坐标：
    //   forward 分量沿 (sin yaw, cos yaw)，right 分量沿 (cos yaw, -sin yaw)
    (sy * fwd + cy * lat, cy * fwd - sy * lat)
}
```

**而且这个补偿必须同时应用到"有真头"和"没真头"两条路径**：

```rust
    // ★ 无真头（合成人机）→ 用「脚 + 身高」估头顶，**也必须叠水平补偿**。
    //   原来只在 p.head 存在时加 lean_shift：人机一探头/趴下，模型的头已经挪出去，
    //   瞄点却还钉在身体中轴上 → 表现就是「人机站着能打头，一探头就打不到」。
    let base = match config.aim_target {
        SnapTarget::Head => {
            let mut h = p.head.unwrap_or_else(|| p.feet.offset_up(config.player_height));
            let (dx, dz) = crate::data::lean_shift(config, p);
            h.x += dx;
            h.z += dz;
            h
        }
        SnapTarget::Feet => p.feet,
    };
```

**探头状态怎么读**：`RoleNetMirror.SyncSideAim @0x1A0`（服务端同步的侧身值，int，±1）。

**必须做平滑**，因为 `SyncSideAim` 是 0→±1 **瞬间跳变**的，而模型的探头动画是渐入的：

```rust
/// 探头量的平滑器（按 ptr 记状态；一阶低通，时间常数 0.18s，跟模型动画近似同步）。
fn smooth_lean(ptr: usize, side: i32) -> f32 {
    let target = if side > 0 { 1.0 } else if side < 0 { -1.0 } else { 0.0 };
    // ...
    let a = 1.0 - (-dt / 0.18).exp();
    e.1 += (target - e.1) * a;
    // ...
}
```

> **通用方法论**：只要你在游戏里看到某个"动画/过渡效果"，而你用的字段是**瞬间跳变**的，就必须加一个和时间常数匹配的平滑器。补偿量跟着跳变，效果会比不做补偿还糟。

## 10.2 人机（AI）不是敌人

**这是本项目最曲折的一段，值得完整讲。**

### 第一次失败：人机完全锁不到

`usable()` 里的过滤是：

```rust
// ❌ 错误版本
if config.aim_enemy_only && !p.enemy {
    return false;                    // 不是敌人 → 跳过
}
```

而 `enemy` 是这样算的：

```rust
// ❌ 错误版本
enemy: !bot && (my_team < 0 || team != my_team),
```

人机（`bot == true`）→ `enemy` 恒为 false → **在任何"只瞄敌人"的配置下，人机一个都锁不到**。

更糟的是，当时 `bot` 的判定依赖菜单里手填的"AI标志偏移"，那个输入框默认是空的：

```rust
pub fn is_bot(ptr: usize) -> bool {
    let off = BOT_FLAG_OFF.load(Ordering::Relaxed);
    if off == 0 || ptr < 0x10000 {
        return false;         // ★ 没配置偏移 → 恒 false → 人机被当成真人
    }
    // ...
}
```

**结果**：人机既不是"敌人"（`enemy=false`），又不是"bot"（`is_bot` 恒 false）→ 两头不靠 → 完全瞄不到。

**日志是唯一的突破口**：每条诊断日志都把人数构成打出来了：

```
[玩家] 共5 敌0 AI5 | AI来源: RoleAIMgr拿到角色=0 直接合成=5
[统计] 总玩家=5 绘制=5 敌=0 AI=5 背敌=0
```

`绘制=5` 说明人机的**坐标和投影完全正常**（ESP 画得好好的），所以问题不在数据链，**纯粹是目标筛选的语义错了**。

### 修法：把人机从"敌我判定"里独立出来

```rust
    // 只瞄敌人：队友排除。
    // 人机（AI）必须单独放行 —— data.rs 里 AI 的 enemy 恒为 false（人机不走队伍阵营判定），
    // 老逻辑「只要 !enemy 就扔」会把整个人机局的目标全部排除 → 人机模式自瞄完全没有目标。
    //
    // ★ 为什么不按队伍号滤人机：日志实测 TeamNum 有 0/1/2，0 是合法队号，
    //   AI 的 TeamId 常和真人同号，按队伍判会把敌人机一起滤掉（静默失灵）——
    //   所以「瞄人机」开着就一律放行。
    if config.aim_enemy_only && !p.enemy && !(config.aim_bots && p.bot) {
        return false;
    }
```

**通用教训**：**当一类目标用"负面条件"被排除时（`!enemy`），你必须确认这类目标真的满足那个正面条件**。人机就是典型的"两边都不算"的第三类。

### 人机的识别：两条路 + 一条退路

```rust
/// 人机候选：StartGame.MyRoleAIMgr → RoleAIManager.logicList。
/// 返回 (BattleRoleLogic指针 或 0, RoleAILogic指针)。
/// 人机在很多模式里不在 StartGame.RoleList，但一定在 AI 管理器里。
fn ai_candidates() -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for a in memory::ai_logic_list(512) {
        if a < 0x10000 { continue; }

        // ★ RoleAILogic.gourdRoleLogic @0x1F0 指回它的 BattleRoleLogic。
        //   拿到了 → 让这台人机走和真人完全一样的路径（能拿到真头、姿态、血量）。
        //   拿不到 → 只能用 RoleAILogic 自带的 Position/Hp/Name 合成一个目标。
        let role = memory::read::<usize>(a + AILOGIC_GOURD_ROLE_OFF).unwrap_or(0);
        if role >= 0x10000 && looks_like_role(role) {
            out.push((role, a));
        } else {
            out.push((0, a));          // 0 表示"没有 BattleRoleLogic，需要合成"
        }
    }
    out
}
```

**合成路径的关键差异**（这个表能帮你理解"为什么人机总是有很多小毛病"）：

| 数据 | 真人 | 人机（有 BattleRoleLogic） | 人机（合成路径） |
|---|---|---|---|
| 位置 | `role+0x7F8` | `role+0x7F8` | `RoleAILogic.Position @0x64`（服务端低频） |
| 真头 | ✅ `headPoint` | ✅ | ❌ 只能用「脚+身高」估 |
| 血量 | `mirror.SyncHp` | ✅ | `AILogic.Hp`（常常读到 0 = 没下发） |
| 姿态 | `mirror.SyncStandbyState` | ✅ | ❌ 硬编码"站立" |
| 探头 | `mirror.SyncSideAim` | ✅ | ❌ 硬编码 0 |

**所以"人机探头打不到头"的根因**是：合成路径的人机**根本读不到探头状态**，`head_shift` 算出来是 `(0,0)`。要真正解决，得从 `ClientRoleAILogic`（人机的**客户端模型**控制器）那条链挖出模型骨骼。

```rust
// ---- ClientRoleAILogic（人机客户端模型控制器）----
// 人机没有 BattleRoleLogic 时，真人的 `RoleControl.headPoint` 链不适用，
// 模型头只能从这边找：
//   RoleAITrans @0x60（Unity Transform）—— 人机模型根节点
//   animator   @0x78 —— 探头/蹲下动画应该在这条链里
pub const CRAI_ANIMATOR_OFF: usize = 0x78;
```

### 一个重要的诊断设计

**当"没瞄上目标"的时候，日志必须能告诉你"为什么"**：

```rust
            // 人机逐个体检：合成人机（只有 RoleAILogic、没有 BattleRoleLogic）在 data.rs 里
            // enemy 恒 false、坐标走 RoleAILogic.Position（服务端同步）。瞄不上时，
            // 必须一眼看出是被 alive / 敌我 / 距离 / 投影 / FOV 里的哪一道挡的。
            let mut extra = String::new();
            let bots: Vec<&Player> = snapshot.players.iter().filter(|p| p.bot).collect();
            if !bots.is_empty() {
                extra.push_str(&format!("  [人机体检] 列表内人机={}", bots.len()));
                for p in bots.iter().take(4) {
                    let d = /* 距离 */;
                    let proj = screen_of(p);
                    let spos = proj.map_or("投影失败".to_string(), |s| format!("({:.0},{:.0})", s.x, s.y));
                    let fov = proj.and_then(&in_fov).map_or("FOV外".to_string(), |x| format!("{x:.0}px"));
                    extra.push_str(&format!(
                        "\n      {} ptr=0x{:X} 距={d:.1}m 屏幕={spos} {fov} 可用={} 敌={} 活={}",
                        p.name_str(), p.ptr, usable(config, snapshot, p), p.enemy, p.alive
                    ));
                }
            }
```

日志长这样，一眼定位：

```
[人机诊断] ptr=0x... pos=(57.00,261.00,95.00) client=0x... vis=1 rot=[...] spot=...
    真头=(57.0,262.3,95.0) ★真人链可用
```

## 10.3 幽灵框：退出的玩家还留在列表里

**现象**：玩家退出/传送走后，他的角色对象**还留在 `RoleList` 里**，位置冻结不动 → ESP 上多出几个"空气框"。

**两道防线**：

```rust
/// 是否在线：`BattleRoleLogic.isOnline @0xB24`。
/// ★ 读不到就当作"在线"（宁可不拦）—— 因为一旦误判，你会把真敌人隐藏掉，
///   那比多画一个空气框严重得多。
pub fn role_is_online(role: usize) -> bool {
    match memory::read::<u8>(role + ROLE_ONLINE_OFF) {
        Some(v) => v != 0,
        None => true,
    }
}
```

```rust
/// 第二道：位置+朝向长期冻结的判定。
/// ★ 默认关闭！因为它分不清"已退出"和"只是站着不动的狙击手"。
///   蹲下(2)/趴下(3) 与人机直接豁免（趴着不动是正常战术）。
fn ghost_check(role: usize, pos: Vec3, yaw: f32, now: u64) -> bool {
    // ...
}
```

**这是一个典型的"权衡"设计**：宁可多画（假阳性），不要少画（假阴性）。**任何"隐藏目标"的过滤都要遵守这个原则。**

## 10.4 加载时崩溃（已在第 3 章详述）

## 10.5 "完全锁不上"的两个隐蔽原因

排查过程中遇到过的、值得记下来的：

**① 距离过近被排除**

```rust
/// 离自己这么近的目标不瞄（重复的自己 / 载具里的自己）。
/// ⚠️ 原来 2.0 米：香肠派对的人机常常是贴脸距离，整批人机会被这条一刀切掉
/// （表现就是「人机就在准心上，准心一点不动」）。
/// 自己已经由 ptr 比对排除了，这里只兜底重复对象。
const MIN_TARGET_DIST: f32 = 0.6;
```

**② FOV 判定用了"瞄点"而不是"身体"**

```rust
    // 身体中心投影：**只用于「能不能入选 FOV」的判定**。
    // 为什么单独要这一份：真人走真头（headPoint），瞄点准；而合成人机拿不到真头，
    // 瞄点只能按「脚 + player_height」估。这 1.5 米一旦估得不准（人机模型身高/
    // Position 基准和真人不同），瞄点会整个掉出 FOV，
    // 表现就是「人机明明在准心上、准心却一点不动」。
    // 入选改用身体中心后，估高偏差只影响最终误差大小，不再决定能不能锁上。
    let center_screen_of = |p: &Player| -> Option<memory::Vec2> {
        let top = p.head.unwrap_or_else(|| p.feet.offset_up(config.player_height));
        let c = memory::Vec3 {
            x: (p.feet.x + top.x) * 0.5,
            y: (p.feet.y + top.y) * 0.5,
            z: (p.feet.z + top.z) * 0.5,
        };
        memory::world_to_screen(c, snapshot.view_matrix, sw, sh, proj)
    };
```

**通用原则**：**"能不能选中"和"瞄得准不准"是两个独立问题，用两个不同的坐标系判断。** 用估算值去做"入选判定"会让误差被放大成"完全没反应"。

---

# 第 11 章 工程纪律：让你能持续维护它

## 11.1 诊断日志是第一生产力

**这个项目的开发过程，本质上是"写日志 → 看日志 → 改代码"的循环。** 几个规则：

**规则 1：每个关键环节都要有限流的诊断日志**

```rust
/// 自瞄诊断日志：限频 1 秒一条，把"为什么没瞄"写清楚。
static AIM_LOG_MS: AtomicU64 = AtomicU64::new(0);

fn aim_due() -> bool {
    let now = t_ms();
    if now.saturating_sub(AIM_LOG_MS.load(Ordering::Relaxed)) < 1000 {
        return false;
    }
    AIM_LOG_MS.store(now, Ordering::Relaxed);
    true
}
```

**规则 2：日志要写"为什么没做"，而不只是"做了什么"**

```rust
    if !config.aim_enabled {
        if aim_due() { aim_note("没效果：总开关『自瞄』没勾"); }
        return;
    }
    if !snapshot.valid {
        if aim_due() { aim_note("暂停：快照无效（矩阵/数据未就绪）"); }
        return;
    }
    if config.menu_open {
        if aim_due() { aim_note("暂停：菜单开着（自瞄在菜单打开时一律不工作）"); }
        return;
    }
```

**为什么**：这样用户看一眼日志就知道"为什么没生效"，不需要问你。而且当**你自己**排查时，也不会陷入"到底哪一步断了"的困境。

**规则 3：把关键数字打出来**

```
[自瞄] 瞄 [人机] 灰郁至痛片段: 距=12.4m 可用=13 FOV内=4 误差=(-3,-5)px 位移=(0,-1)
```

这一行里有：目标类型、距离、可用目标数、FOV 内数量、屏幕误差、实际位移。**任何一个数字异常，都能立刻指向某个环节。**

## 11.2 版本指纹：一个真实踩过的坑

**问题**：项目里有多个 DLL（`xcpd.dll` / `xcpd_next.dll`，以及 debug/release 两个构建），日志头里的版本号是写死的字符串。有一次改完代码测试"没效果"，排查半天才发现**注入器加载的是另一个 DLL**。

**解决方案**：把版本号当成**构建指纹**，每次改动都改：

```rust
    log::reset("xcpd ESP 启动 【v126·人机自瞄放行+人机诊断】");
```

**使用规则**：改完代码 → 改版本号 → 重新构建 → 注入 → **第一件事是看日志第一行是不是新版本号**。不是的话，说明注入的不是你刚构建的文件。

**这条纪律能省掉你无数个"明明改了却没效果"的困惑。**

## 11.3 偏移表集中管理（第 1 章已述）

## 11.4 死代码清理：让编译器当你的清道夫

项目做久了一定会积累大量"挖偏移用的一次性脚本"。手工找很痛苦，**让编译器告诉你**：

```bash
# 导出所有死代码清单（--message-format=short 会输出单行格式，方便处理）
cargo check --message-format=short 2>&1 | Select-String 'never used|never read|never constructed'
```

然后按行号批量删除。本项目实测：**一次清理掉了 135 个顶层项、约 5000 行**（占总量的 59%）。

**注意一个进阶技巧**：删完一轮后要**再扫一次**——因为"只被死函数引用的函数"会在第一轮之后变成新的死代码。本项目清理了三轮才归零。

**另一类"僵尸代码"编译器抓不到**：`pub` 的 static 变量，**只有写、没有读**：

```rust
// ❌ 编译器不报错（因为 pub），但实际是死的：
pub static DRAW_DISTANCE: AtomicU32 = AtomicU32::new(0);
// 全项目只有 lib.rs 里一句 .store()，没有任何地方 .load()
```

**怎么发现**：用 grep 搜每个 `pub static` 的名字，看除了定义和 `.store()` 之外，有没有 `.load()` 的读取点。没有就是僵尸——通常是被清理掉的旧功能的残留。

## 11.5 配置项的组织

```rust
pub struct Config {
    pub aim_enabled: bool,      // 自瞄总开关
    pub aim_fov: f32,           // FOV 半径（像素）
    pub aim_smooth: f32,        // 平滑系数（越大越平滑、增益越低）
    // ...
}
```

**设计原则**：
- **只放"用户会调"的**。如果一个参数你自己都不确定是干嘛的，它不该出现在菜单里。
- **危险功能（写内存）默认全关**，并且代码里留风险注释。
- **数值型配置给合理默认值**，让"什么都不动"也能正常用。

本项目后来做了一次大幅精简：**菜单从 84 个控件压到 27 个**。判断标准很简单——**"这个开关我上次用是什么时候？"想不起来的，全删。**

---

# 第 12 章 迁移到其他游戏的 Checklist

这一章是本文档最想给你的东西。**换一个游戏时，按这个顺序做，能少走 80% 的弯路。**

## 12.1 第一阶段：探路（1~2 小时）

| 步骤 | 做什么 | 判断标准 |
|---|---|---|
| 1 | 确认引擎 | 看进程目录有没有 `UnityPlayer.dll` + `GameAssembly.dll`（→ Unity IL2CPP）；只有 `UnityPlayer.dll` 没有 `GameAssembly.dll`（→ 老的 Mono 后端，另一套方法）；`*-Win64-Shipping.exe`（→ UE） |
| 2 | 跑 Il2CppDumper | 拿到 `dump.cs`。**能 dump 出来就成功了一半** |
| 3 | 在 dump 里搜关键词 | 搜 `Role` / `Player` / `Entity` / `Avatar` / `Character`，找那个"每个玩家一个实例"的类 |
| 4 | 找到它的**类名** | 这个名字会是你后面所有工作的核心（本项目是 `BattleRoleLogic`） |

**探路失败的信号**：Il2CppDumper 跑不出结果（元数据被加密/混淆）、搜不到任何像玩家的类名。→ 这个游戏不适合用本文方法，及早止损。

## 12.2 第二阶段：打通数据链（最容易卡住的阶段）

| 步骤 | 做什么 | 关键技巧 |
|---|---|---|
| 5 | 挂上渲染 | 用 hudhook 最小示例，先做到"能在游戏里画一个红点" |
| 6 | 拿到模块基址 | `GetModuleHandleA("GameAssembly.dll")` |
| 7 | **找到角色列表** | 试本文第 5 章的三条路。**优先试"按类名枚举"** |
| 8 | **验证列表** | 打日志：数量对不对？（一局应该 = 玩家数 + 自己）类名分布对不对？ |
| 9 | 找坐标字段 | 在 dump 里搜 `Vector3` 字段，**逐个试**，用本文 6.3 的方法验证 |
| 10 | **验证坐标** | 打日志：值在合理范围内吗？走动时会变吗？ |

**这一步的成败关键是"日志"**：宁可多打日志看，也不要凭 dump 猜。

**卡住的典型原因**：
- 坐标为 0 或不变化 → 字段选错了，或者要读的是"实时位置"而不是"落地位置"
- 列表数量为 0 → 静态链错了（试试按类名枚举）
- 数量对但坐标全是垃圾 → 忘了做类名校验，捞到了一堆非玩家对象

## 12.3 第三阶段：ESP

| 步骤 | 做什么 | 关键技巧 |
|---|---|---|
| 11 | 找视图矩阵 | UnityPlayer.dll 里的静态链，或全内存扫一个"像矩阵的 16 个 float" |
| 12 | **写矩阵校验** | 反推相机位置 + 看它是否在玩家附近（本文 7.2） |
| 13 | 试 4 种组合 | 行主序/列主序 × 翻转/不翻转。**这一步别偷懒，一定要挨个试** |
| 14 | 画框 | 用本文 8.1 的"比例框"算法，比世界四角稳得多 |
| 15 | 加防御 | 坐标非法就不画（本文 8.2） |

## 12.4 第四阶段：自瞄

| 步骤 | 做什么 | 关键技巧 |
|---|---|---|
| 16 | 屏幕误差 → 鼠标位移 | 先用固定系数跑通 |
| 17 | **换成自适应系数** | 本文 9.3 的 K 学习算法（**不要**去猜玩家的灵敏度） |
| 18 | 加死区/限步/增益 | 本文 9.4 |
| 19 | 加目标粘滞 | 本文 9.5 |
| 20 | 加超前补偿 | 本文 9.6 |

## 12.5 第五阶段：打磨（时间黑洞，但决定好不好用）

- **蹲下/趴下**：找姿态字段，让框和瞄点跟着变
- **探头**：找侧身字段，加横向补偿（本文 10.1）
- **人机/AI**：找 AI 管理器（本文 10.2）
- **幽灵框**：找 `isOnline` / 位置冻结检测（本文 10.3）
- **加载崩溃**：加 API 闸门（本文第 3 章）

## 12.6 不同引擎的差异速查

| 引擎 | 元数据来源 | 找对象 | 备注 |
|---|---|---|---|
| **Unity IL2CPP**（本文） | `GameAssembly.dll` + `global-metadata.dat` | IL2CPP 导出 API + 类名过滤 | 最容易，有官方元数据 |
| Unity Mono | `Assembly-CSharp.dll`（托管程序集） | 反射读取，甚至能直接调 C# 方法 | 更简单，但现代游戏基本不用了 |
| Unreal Engine | `UE4/5-*.exe` | **没有类名元数据**，靠 UObject 的 `GNames` 数组反查 | 难在"找对象"，通常用 `GObjects` 数组 + `GetName` 虚函数 |
| 自研引擎 | 什么都没有 | 纯结构扫描（本文路线③） | 最费时，靠"蹲下差异法"这类手段一个个试 |

## 12.7 什么时候该放弃

**及早止损的信号**（别投入几百小时才发现走不通）：

- 有内核级反作弊（EAC / BE / Vanguard / 各类国产驱动级）→ **注入这一关就过不去**，不值得继续
- 元数据被强加密，Il2CppDumper 完全跑不出来
- 坐标不在客户端内存里（纯服务器权威 + 本地只存渲染插值数据）→ 你能读到的东西不足以还原出敌人位置
- 你自己在游戏里被高频封号 → 说明检测已经覆盖你的手法

---

# 第 13 章 排查表：症状 → 原因 → 动作

**按这个表查，能覆盖 95% 的问题。**

| 症状 | 最可能的原因 | 动作 |
|---|---|---|
| 游戏直接闪退（启动时） | 在加载阶段调了 IL2CPP API | 检查所有 API 调用点是否都有 `api_ok()` 闸门（第 3 章） |
| 游戏闪退（运行中偶发） | 矩阵读到垃圾 → 坐标 1e30 → ImGui 崩 | 加 `matrix_ok` 校验 + 绘制前的 `sane()` 检查 |
| 一个框都没有 | ① 列表为空 ② 类名变了 ③ 矩阵没拿到 | 看日志的"类名分布"和"原始/采用"数字 |
| 框在错误的位置 | 行主序/列主序猜错、Y 轴没翻转 | 试那 4 种组合；用 `翻 X`/`翻 Y` 开关现场调 |
| 框跟着自己转（画到了自己身上） | 列表里包含了"自己" | 用 `isLocalPlayer` 或 ptr 比对排除自己 |
| 框的数量比实际玩家多 | 幽灵框（离线玩家留在列表里） | 加 `isOnline` 过滤（第 10.3） |
| 框一卡一卡 | 坐标是低频同步的（人机/远距离玩家） | 加位置平滑（第 8.4） |
| 蹲下时框停在头上方 | 没用姿态字段压矮框 | 挖 `stance` 字段（第 6.3 方法 A） |
| 自瞄完全没反应 | ① 总开关没开 ② 菜单开着 ③ 没按触发键 ④ 目标全被过滤 | **看日志**——每条日志都写明了原因（第 11.1 规则 2） |
| 自瞄能锁但准心不动 | 误差小于死区 / FOV 判定用错了坐标系 | 第 10.5 |
| 自瞄冲过头来回摆 | 增益太大 / K 学习率太高 | 调低 `frac` 上限和 K 的 EMA 系数（第 9.3~9.4） |
| 自瞄总落在人身后 | 闭环稳态滞后 | 加超前补偿（第 9.6） |
| 自瞄在两个人之间横跳 | 每帧重选最近目标 | 加目标粘滞（第 9.5） |
| 人机完全瞄不到 | `enemy` 语义把 AI 排除了 | 第 10.2 |
| 人机探头打不到头 | 合成路径读不到探头状态 | 第 10.2 末尾（要从 `ClientRoleAILogic` 挖） |
| 改了代码但"没效果" | 注入的不是新构建的 DLL | **看日志第一行的版本指纹**（第 11.2） |
| 每个敌人画两遍框 | 注入了两份 DLL | 加单实例锁（第 2.4） |
| 菜单中文是方块 | 用了 ImGui 默认字体（不含中文） | 加载系统中文字体（第 2.2） |
| CPU 占用极高 / 卡顿 | 有无条件的全内存扫描 | 给兜底扫描加开关+限频（第 5.2） |

---

# 附录

## 附录 A：最小可运行骨架（可直接复制起步）

```
Cargo.toml          → 见第 1.1 节
src/lib.rs          → 第 2 章的 DllMain + EspLoop + run()
src/memory.rs       → 第 3 章的 read/is_readable + 第 4 章的 il2cpp_* + 第 7 章的 project_ndc
src/offset.rs       → 先留空，挖一个填一个
src/data.rs         → 先只做"按类名枚举 + 读坐标"
src/esp.rs          → 先只画方框
src/aim.rs          → 先不做，ESP 跑通再说
src/config.rs       → 只放开关
src/log.rs          → 第 6.5 节的限流日志
```

**推荐的实现顺序**（每步都要能跑起来再进下一步）：

1. 能画一个红点（验证 hook）
2. 能列出所有 `BattleRoleLogic` 对象的地址（验证 IL2CPP）
3. 能打印出它们的坐标（验证偏移）
4. 能画出一个跟随敌人的框（验证矩阵 + 投影）
5. 加上自瞄（先固定系数，再换自适应）
6. 最后才做那些边界情况

## 附录 B：常用 Win32 调用速查

```rust
// 取模块基址 —— 所有 RVA 都要加它才能得到绝对地址
GetModuleHandleA(c"GameAssembly.dll".as_ptr())

// 取导出函数地址
GetProcAddress(module, c"il2cpp_domain_get".as_ptr() as *const u8)

// 读内存（自己进程也用它，因为带电页保护检查）
ReadProcessMemory(GetCurrentProcess(), addr as *const c_void, buf, size, &mut got)

// 键盘状态（自瞄触发键、菜单键）
(GetAsyncKeyState(VK_RBUTTON as i32) as u16 & 0x8000) != 0
//                                                     ↑ 0x8000 = 当前按下

// 鼠标移动（相对位移，模拟真人）
SendInput(1, &input, size_of::<INPUT>())
//         dwFlags = MOUSEEVENTF_MOVE

// 解除鼠标锁定（打开菜单时要）
ClipCursor(std::ptr::null())
```

## 附录 C：术语表

| 术语 | 含义 |
|---|---|
| **RVA** | 相对虚拟地址。模块基址 + RVA = 绝对地址 |
| **偏移 / offset** | 结构体内部字段相对于对象起始地址的字节距离 |
| **指针链** | 一串解引用：`base → +0x10 → +0x20 → 得到目标` |
| **静态字段** | IL2CPP 里属于类而非实例的字段，通常存放"单例管理器"，是找一切的起点 |
| **Il2CppClass** | 类的元数据对象。**每个 IL2CPP 对象的第 0 个字段都指向它**（这是类名校验的原理） |
| **dump** | 用 Il2CppDumper 导出的类结构文本（类名、字段名、偏移） |
| **Present Hook** | 挂钩 DXGI 的 Present 函数，在游戏提交画面后插入自己的绘制 |
| **FOV** | Field of View，这里指"自瞄生效的屏幕圆形范围半径（像素）" |
| **ESP** | Extra Sensory Perception，指透视画框这类视觉增强 |
| **死区 (dead zone)** | 小于这个误差就不动，避免抖动 |
| **粘滞 (sticky)** | 倾向于保持当前目标，避免在多个目标间横跳 |

## 附录 D：本文档对应的真实项目

| 项目 | 说明 |
|---|---|
| 目标游戏 | 香肠派对（Sausage Man，Unity IL2CPP） |
| 语言 / 库 | Rust 2024 · hudhook 0.9 · imgui 0.12 · windows-sys 0.61 |
| 最终规模 | 8 个模块、约 4200 行（清理掉 5100 行死代码之后） |
| 核心偏移 | 见 `offset.rs`，每条都带实测来源注释 |

**读完之后建议的做法**：拿一个你熟悉的小游戏（最好是单机、无强反作弊的 Unity 游戏）从附录 A 的顺序走一遍。**走通一次完整的链路，比读十遍文档有用得多。**

---

*本文档所有偏移、日志、代码片段均来自真实项目的实测结果。偏移值会随游戏版本失效，但方法论长期有效。*
