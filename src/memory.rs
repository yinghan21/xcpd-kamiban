use hudhook::hooks::find_process_hwnd;
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowA, IsWindow, GetClientRect};
use windows_sys::s;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery };
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, PAGE_NOACCESS, PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY,
    PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY,
};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use std::sync::OnceLock;

/// 单调毫秒计时（不依赖 windows-sys 额外 feature）。
fn ticks_ms() -> u64 {
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

// ===================== IL2CPP API 调用闸门 =====================
// 背景（实测）：在“出生岛/上飞机/跳伞”这些**加载/切场景**时刻调 IL2CPP API
// （il2cpp_class_from_name / il2cpp_class_get_name）会拿到已失效的元数据指针，
// 之后一旦解引用就访问违例闪退（实测崩在 xcpd.dll+0x8206b = il2cpp_class_get_name_api 偏移 0x8B）。
//
// 对策：渲染线程每帧调 `api_tick()` 打时间戳；**加载时 Present 不会跑**，
// 时间戳自然变旧 → 所有 API 调用被拒绝（返回 None），等回到能渲染的帧再恢复。
static API_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 渲染线程每帧调用：标记“现在游戏能正常渲染，元数据是稳定的”。
pub fn api_tick() {
    API_TICK.store(ticks_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// 现在能否安全调 IL2CPP API（超过 400ms 没渲染过 = 正在加载，拒绝）。
fn api_ok() -> bool {
    let t = API_TICK.load(std::sync::atomic::Ordering::Relaxed);
    t != 0 && ticks_ms().saturating_sub(t) < 400
}
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::ffi::c_void;

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct Vec3 {
    pub x: f32,
    pub z: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    pub fn to_array(self) -> [f32; 2] {
        [self.x, self.y]
    }
}

impl Vec3 {
    /// 沿世界高度轴（第 3 个分量）平移。
    /// 用来从脚坐标推出头顶（身高）。
    pub fn offset_up(self, d: f32) -> Self {
        Self {
            x: self.x,
            z: self.z,
            y: self.y + d,
        }
    }
}

/// 投影参数。矩阵/坐标对不上时，用菜单里的开关现场试，不用重编译。
#[derive(Copy, Clone, Debug, Default)]
pub struct Projection {
    /// true = 行为主序（转置）解释矩阵
    pub row_major: bool,
    /// 屏幕 X 取反
    pub flip_x: bool,
    /// 屏幕 Y 取反
    pub flip_y: bool,
}

pub fn UnityPlayer_module_base() -> Option<usize> {
    let name = s!("UnityPlayer.dll");
    let handle = unsafe { GetModuleHandleA(name) };
    if handle.is_null() {
        return None;
    }
    Some(handle as usize)
}

pub fn GameAssembly_module_base() -> Option<usize> {
    let name = s!("GameAssembly.dll");
    let handle = unsafe { GetModuleHandleA(name) };
    if handle.is_null() {
        return None;
    }
    Some(handle as usize)
}

pub fn is_readable(addr: usize, size: usize) -> bool {
    if addr == 0 || size == 0 {
        return false;
    }
    let Some(end) = addr.checked_add(size) else {
        return false;
    };

    let mut a = addr;
    let mut mbi = unsafe { std::mem::zeroed::<MEMORY_BASIC_INFORMATION>() };
    let mbi_size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();

    while a < end {
        let v = unsafe { VirtualQuery( a as *const c_void, &mut mbi, mbi_size) };
        if v == 0 {
            return false;
        }
        if mbi.State != MEM_COMMIT {
            return false;
        }

        if mbi.Protect & (PAGE_NOACCESS | PAGE_GUARD) != 0 {
            return false;
        }

        let regtion_end = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize);
        if regtion_end == 0 {
            return false;
        }
        a = regtion_end;
    }
    true
}

/// 读目标内存。用 `ReadProcessMemory`（异常安全）——
/// 避免“先判可读、再直接解引用”时被游戏抢先释放/改保护 → 访问违例把整个游戏弄崩。
pub fn read<T: Copy>(addr: usize) -> Option<T> {
    let sz = std::mem::size_of::<T>();
    if addr < 0x1000 {
        return None;
    }
    let mut out: T = unsafe { std::mem::zeroed() };
    let mut got = 0usize;
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as *const c_void,
            &mut out as *mut T as *mut c_void,
            sz,
            &mut got,
        )
    };
    if ok != 0 && got == sz {
        Some(out)
    } else {
        None
    }
}

/// 读“连排”Vector3：x@0、y@4、z@8（共 12 字节）。用于角色坐标 `$t @0x7F8`。
pub fn read_vec3_contig(addr: usize) -> Option<Vec3> {
    let mut b = [0u8; 12];
    if safe_read_upto(addr, &mut b) < 12 {
        return None;
    }
    let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let (x, y, z) = (f(0), f(4), f(8));
    if x.is_finite() && y.is_finite() && z.is_finite() {
        Some(Vec3 { x, y, z })
    } else {
        None
    }
}

/// 读 IL2CPP `System.String` 字段（字段存的是对象指针）→ `String`。
/// 布局：klass@0 monitor@8 length(i32)@0x10 UTF-16 字符@0x14。
pub fn read_il2cpp_string(field_addr: usize) -> Option<String> {
    let sp = read::<usize>(field_addr)?;
    if sp < 0x10000 {
        return None;
    }
    let len = read::<i32>(sp + 0x10)?;
    if len <= 0 || len > 64 {
        return None;
    }
    let mut buf = vec![0u8; (len as usize) * 2];
    if safe_read_upto(sp + 0x14, &mut buf) < buf.len() {
        return None;
    }
    let mut s = String::new();
    for ch in buf.chunks_exact(2) {
        let u = u16::from_le_bytes([ch[0], ch[1]]);
        if u == 0 {
            break;
        }
        if let Some(c) = char::from_u32(u as u32) {
            s.push(c);
        }
    }
    Some(s)
}

fn self_process() -> *mut c_void {
    unsafe { GetCurrentProcess() }
}

/// 用 ReadProcessMemory 读自己进程，允许部分成功，返回实际读到的字节数。
pub fn safe_read_upto(addr: usize, buf: &mut [u8]) -> usize {
    let h = self_process();
    let mut read = 0usize;
    unsafe {
        ReadProcessMemory(
            h,
            addr as *const c_void,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            &mut read,
        );
    }
    read
}

fn norm3(v: [f32; 3]) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

/// 从「行向量约定 + 列主序存储」的视图投影矩阵反推相机的世界坐标。
///
/// 原理：x 行 / y 行 / w 行的前三个元素分别是 A*right、B*up、forward
/// （A、B 是投影的缩放系数，forward 是单位向量），而
///   m12 = -A*(right·eye)、m13 = -B*(up·eye)、m15 = -(forward·eye)
/// 所以把这三个投影值乘回单位基向量相加就能还原 eye。
///
/// 坐标系与实体坐标一致：字段 (x, y, z) 依次对应矩阵公式里的 (world.x, world.y, world.z)。
pub fn camera_position(matrix: [f32; 16]) -> Option<Vec3> {
    let m = matrix;

    let x_row = [m[0], m[4], m[8]];
    let y_row = [m[1], m[5], m[9]];
    let w_row = [m[3], m[7], m[11]];

    let a = norm3(x_row);
    let b = norm3(y_row);
    let c = norm3(w_row);
    if a < 1e-6 || b < 1e-6 || c < 1e-6 {
        return None;
    }

    let r = [x_row[0] / a, x_row[1] / a, x_row[2] / a];
    let u = [y_row[0] / b, y_row[1] / b, y_row[2] / b];
    let f = [w_row[0] / c, w_row[1] / c, w_row[2] / c];

    let dr = -m[12] / a;
    let du = -m[13] / b;
    let df = -m[15] / c;

    Some(Vec3 {
        x: dr * r[0] + du * u[0] + df * f[0],
        y: dr * r[1] + du * u[1] + df * f[1],
        z: dr * r[2] + du * u[2] + df * f[2],
    })
}

/// 世界坐标 -> NDC。返回 (ndc_x, ndc_y, w)。
///
/// - 列主序(Unity/D3D 常见)：每一项取“列”的前三个元素 (m0,m4,m8 / m1,m5,m9)。
/// - 行主序(转置)：每一项取“行”的前三个元素 (m0,m1,m2 / m4,m5,m6)。
pub fn project_ndc(world: Vec3, matrix: [f32; 16], opt: Projection) -> Option<(f32, f32, f32)> {
    let m = matrix;

    let (x, y, w) = if opt.row_major {
        (
            world.x * m[0] + world.y * m[1] + world.z * m[2] + m[3],
            world.x * m[4] + world.y * m[5] + world.z * m[6] + m[7],
            world.x * m[12] + world.y * m[13] + world.z * m[14] + m[15],
        )
    } else {
        (
            world.x * m[0] + world.y * m[4] + world.z * m[8] + m[12],
            world.x * m[1] + world.y * m[5] + world.z * m[9] + m[13],
            world.x * m[3] + world.y * m[7] + world.z * m[11] + m[15],
        )
    };

    // 注意: 原来是 `w < 0.001 && !w.is_finite()`，两个条件几乎不可能同时成立，
    // 等于没过滤，相机背后(w<=0)的点会算成巨大坐标。
    // 用 !(w > 0.001) 同时覆盖 w<=0、w 极小 和 NaN。
    if !(w > 0.001) {
        return None;
    }

    let ndc_x = x / w;
    let ndc_y = y / w;
    if !ndc_x.is_finite() || !ndc_y.is_finite() {
        return None;
    }

    Some((ndc_x, ndc_y, w))
}

pub fn world_to_screen(
    world: Vec3,
    matrix: [f32; 16],
    width: f32,
    height: f32,
    opt: Projection,
) -> Option<Vec2> {
    let (ndc_x, ndc_y, _w) = project_ndc(world, matrix, opt)?;

    let center_x = width * 0.5;
    let center_y = height * 0.5;

    let sx = if opt.flip_x {
        center_x - ndc_x * center_x
    } else {
        center_x + ndc_x * center_x
    };
    let sy = if opt.flip_y {
        center_y + ndc_y * center_y
    } else {
        center_y - ndc_y * center_y
    };

    Some(Vec2::new(sx, sy))
}

/// 上次全局扫描定位到的「视图投影矩阵」绝对地址（0 = 未知）。
/// 指针链失效时用它兜底，省得每次都全内存重扫。
/// 注意：地址只在本局有效，换局/地址变了要重新扫描。
pub static MATRIX_ADDR: AtomicUsize = AtomicUsize::new(0);

/// 结构上像不像一个「行向量约定」的视图投影矩阵。
///
/// 判据（用交接里那一帧"曾经正常"的矩阵验证过）：
/// - 三个基向量行两两正交（|dot| < 0.15）；
/// - forward 行（m[3],m[7],m[11]）是单位向量；
/// - x 行 / y 行长度比 ≈ 屏幕宽高比（16:9 → 1.78）；
/// - 带非零平移（相机不在世界原点）。
///
/// 这一条会**拒掉 identity 矩阵** —— 正好就是现在 p2+0x100 读到的那坨单位矩阵。
pub fn matrix_looks_valid(m: &[f32; 16]) -> bool {
    for &v in m.iter() {
        if !v.is_finite() || v.abs() > 1.0e7 {
            return false;
        }
    }

    let x = [m[0], m[4], m[8]];
    let y = [m[1], m[5], m[9]];
    let w = [m[3], m[7], m[11]];

    let nx = norm3(x);
    let ny = norm3(y);
    let nw = norm3(w);
    if nx < 1.0e-3 || ny < 1.0e-3 {
        return false;
    }
    // forward 行必须是单位向量（透视投影的行向量约定）
    if !(0.8..=1.25).contains(&nw) {
        return false;
    }
    // y 行 / x 行 长度比 = 屏幕宽高比
    if !(0.8..=2.8).contains(&(ny / nx)) {
        return false;
    }

    let dot = |a: [f32; 3], b: [f32; 3]| {
        (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]) / (norm3(a) * norm3(b))
    };
    if dot(x, y).abs() > 0.15 || dot(x, w).abs() > 0.15 || dot(y, w).abs() > 0.15 {
        return false;
    }

    // 真 VP 矩阵一定带平移；identity(0,0,0,1) 在这里被排除。
    if m[12].abs() + m[13].abs() + m[14].abs() < 1.0 {
        return false;
    }

    true
}

/// 按指针链收集视图矩阵候选（带内容校验）。
/// 中间那一跳（p1 → 相机对象）实测在 0x38 / 0x40 之间变过，所以两个都试。
/// 返回所有“结构上像 VP 矩阵”的候选，由调用方（用相机是否贴着自己）再筛。
pub fn chain_view_matrix_candidates(module: usize) -> Vec<[f32; 16]> {
    let mut out: Vec<[f32; 16]> = Vec::new();
    let Some(p0) = read::<usize>(module + crate::offset::VIEW_MATRIX) else {
        return out;
    };
    let Some(p1) = read::<usize>(p0 + crate::offset::VIEW_MATRIX_OFFSET_1) else {
        return out;
    };
    for hop in [crate::offset::VIEW_MATRIX_OFFSET_2, 0x38usize] {
        if let Some(p2) = read::<usize>(p1 + hop) {
            if let Some(m) = read::<[f32; 16]>(p2 + crate::offset::VIEW_MATRIX_OFFSET_3) {
                if matrix_looks_valid(&m) && !out.contains(&m) {
                    out.push(m);
                }
            }
        }
    }
    out
}

/// 读上次全局扫描缓存的矩阵地址（0 时返回 None）。
pub fn cached_view_matrix() -> Option<[f32; 16]> {
    let a = MATRIX_ADDR.load(AtomicOrdering::Relaxed);
    if a == 0 {
        return None;
    }
    read::<[f32; 16]>(a)
}

/// 遍历所有可写内存块。
fn for_each_region_chunk(mut f: impl FnMut(usize, &[u8])) {
    const CHUNK: usize = 1 << 20;
    const OVER: usize = 64;
    let mut buf = vec![0u8; CHUNK + OVER];
    let mbi_size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    let mut mbi = unsafe { std::mem::zeroed::<MEMORY_BASIC_INFORMATION>() };
    let mut addr: usize = 0x10000;
    let limit: usize = 0x0000_7FFF_FFFF_0000;
    while addr < limit {
        let ok = unsafe { VirtualQuery(addr as *const c_void, &mut mbi, mbi_size) };
        if ok == 0 {
            break;
        }
        let base = mbi.BaseAddress as usize;
        let size = mbi.RegionSize;
        if size == 0 {
            break;
        }
        let writable = mbi.State == MEM_COMMIT
            && mbi.Protect
                & (PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)
                != 0
            && mbi.Protect & (PAGE_NOACCESS | PAGE_GUARD) == 0;
        if writable {
            let mut off = 0usize;
            while off < size {
                let n = std::cmp::min(CHUNK + OVER, size - off);
                let got = safe_read_upto(base + off, &mut buf[..n]);
                if got > 0 {
                    f(base + off, &buf[..got]);
                }
                off += CHUNK;
            }
        }
        let next = base.saturating_add(size);
        if next <= addr {
            break;
        }
        addr = next;
    }
}

/// 全内存搜所有落在 [lo,hi] 的 u64 值，返回 (存放地址, 值)。
pub fn scan_u64_in_range(lo: u64, hi: u64, cap: usize) -> Vec<(usize, u64)> {
    let mut hits: Vec<(usize, u64)> = Vec::new();
    for_each_region_chunk(|addr, b| {
        if hits.len() >= cap {
            return;
        }
        let mut i = 0usize;
        while i + 8 <= b.len() {
            let v = u64::from_le_bytes([
                b[i], b[i + 1], b[i + 2], b[i + 3], b[i + 4], b[i + 5], b[i + 6], b[i + 7],
            ]);
            if v >= lo && v <= hi {
                hits.push((addr + i, v));
            }
            i += 8;
        }
    });
    hits
}

/// 判断一个地址是否像 Il2CppClass 的“类名指针”：*(klass+0x10) 应指向一个短的可打印字符串。
#[allow(dead_code)]
fn class_name_ptr_valid(klass: usize) -> bool {
    let Some(np) = read::<usize>(klass + 0x10) else {
        return false;
    };
    if np < 0x10000 {
        return false;
    }
    let mut n = 0usize;
    for i in 0..64 {
        match read::<u8>(np + i) {
            Some(0) => return n >= 2,
            Some(b) if b.is_ascii_graphic() || b == b' ' => n += 1,
            _ => return false,
        }
    }
    n >= 2
}

/// 全内存类名普查：统计每个类有多少个实例对象，按数量降序返回 (类名, 数量)。
#[allow(dead_code)]
pub fn census_class_names(cap: usize) -> Vec<(String, usize)> {
    use std::collections::HashMap;
    let mut cache: HashMap<usize, Option<String>> = HashMap::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for_each_region_chunk(|_addr, b| {
        let n = b.len() / 8;
        for i in 0..n {
            let off = i * 8;
            let q = u64::from_le_bytes([
                b[off], b[off + 1], b[off + 2], b[off + 3], b[off + 4], b[off + 5], b[off + 6],
                b[off + 7],
            ]);
            if !(0x7F00_0000_0000..0x8000_0000_0000).contains(&q) {
                continue;
            }
            let k = q as usize;
            let name = cache.entry(k).or_insert_with(|| read_klass_name(k));
            if let Some(nm) = name {
                *counts.entry(nm.clone()).or_insert(0) += 1;
            }
        }
    });
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v.truncate(cap);
    v
}

/// ===== IL2CPP 官方 API：按名字找类，再扫实例 =====
type FnDomainGet = unsafe extern "system" fn() -> *mut c_void;
type FnDomainGetAssemblies =
    unsafe extern "system" fn(*mut c_void, *mut usize) -> *mut *mut c_void;
type FnAssemblyGetImage = unsafe extern "system" fn(*mut c_void) -> *mut c_void;
type FnClassFromName =
    unsafe extern "system" fn(*mut c_void, *const u8, *const u8) -> *mut c_void;

/// 用 IL2CPP 导出 API 按（命名空间, 类名）找 Il2CppClass*。
pub fn il2cpp_find_class(namespace: &str, name: &str) -> Option<usize> {
    // 加载/切场景时一律不调 API（元数据可能正在重建）
    if !api_ok() {
        return None;
    }
    let module = GameAssembly_module_base()? as *mut c_void;
    unsafe {
        let domain_get: FnDomainGet =
            std::mem::transmute(GetProcAddress(module, c"il2cpp_domain_get".as_ptr() as *const u8)?);
        let get_asms: FnDomainGetAssemblies = std::mem::transmute(GetProcAddress(
            module,
            c"il2cpp_domain_get_assemblies".as_ptr() as *const u8,
        )?);
        let asm_img: FnAssemblyGetImage = std::mem::transmute(GetProcAddress(
            module,
            c"il2cpp_assembly_get_image".as_ptr() as *const u8,
        )?);
        let cls_from_name: FnClassFromName = std::mem::transmute(GetProcAddress(
            module,
            c"il2cpp_class_from_name".as_ptr() as *const u8,
        )?);

        let domain = domain_get();
        if domain.is_null() {
            return None;
        }
        let mut count: usize = 0;
        let arr = get_asms(domain, &mut count);
        if arr.is_null() || count == 0 {
            return None;
        }
        let cname = std::ffi::CString::new(name).ok()?;
        let cns = std::ffi::CString::new(namespace).ok()?;
        for i in 0..count {
            // 用 ReadProcessMemory 读程序集指针（原来是 `*arr.add(i)` 裸解引用，
            // 程序集数组被回收/重建时会崩）
            let Some(asm) = read::<usize>((arr as usize) + i * 8) else {
                continue;
            };
            if asm == 0 {
                continue;
            }
            let img = asm_img(asm as *mut c_void);
            if img.is_null() {
                continue;
            }
            // 每读一个都再确认一次：加载开始就立即收手
            if !api_ok() {
                return None;
            }
            let k = cls_from_name(img, cns.as_ptr() as *const u8, cname.as_ptr() as *const u8);
            if !k.is_null() {
                return Some(k as usize);
            }
        }
    }
    None
}

/// 用 IL2CPP API 读类的名字（验证用）。
/// ⚠️ 原实现用 `*p.add(i)` 裸指针读返回的类名字符串：
/// 切场景/预匹配阶段如果 `klass` 是垃圾（对象头未初始化），API 会返回一个坏指针，
/// 裸读直接访问违例 → **崩游戏**（实测崩在 xcpd.dll+0x8206b = 本函数偏移 0x8B）。
/// 现在：① 先校验 klass 在合法地址区间且可读；② 字符串全程用 ReadProcessMemory 读。
pub fn il2cpp_class_get_name_api(klass: usize) -> Option<String> {
    // ⓪ 只在地上能渲染的稳定帧里调 API（加载/切场景时一律拒绝）
    if !api_ok() {
        return None;
    }
    // ① klass 必须是合法的“指针”取值，且在能读到的内存里（有 vtable/klass 头）
    if klass < 0x10000 || klass > 0x7FFF_FFFF_FFFF {
        return None;
    }
    if !is_readable(klass, 0x20) {
        return None;
    }
    type Fn = unsafe extern "system" fn(usize) -> *const u8;
    let module = GameAssembly_module_base()? as *mut c_void;
    let f: Fn = unsafe {
        std::mem::transmute(GetProcAddress(
            module,
            c"il2cpp_class_get_name".as_ptr() as *const u8,
        )?)
    };
    let p = unsafe { f(klass) };
    let pa = p as usize;
    if pa < 0x10000 || pa > 0x7FFF_FFFF_FFFF {
        return None;
    }
    // ② 用 ReadProcessMemory 一次性读，坏指针只会读失败（返回 None），不会崩
    let mut buf = [0u8; 128];
    let n = safe_read_upto(pa, &mut buf);
    if n == 0 {
        return None;
    }
    let end = buf[..n].iter().position(|&b| b == 0).unwrap_or(n);
    if end == 0 {
        return None;
    }
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// 权威静态链（全程只用官方静态字段 API + 固定偏移，不扫描）：
/// `GameWorldClientManager.MyGameWorld`(static) → `BattleWorld.startGame`(+0x4C8)
/// → `StartGame.RoleList`(+0x50, `List<BattleRoleLogic>`) → 元素。
/// 返回 (角色对象指针列表, 诊断文本)。
/// 缓存的 startGame 指针：有它就不必每次调用 il2cpp API（高频调 API 容易把游戏弄崩）
static CACHED_SG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// 缓存年龄（调用次数），用于“只有 1 个角色”时定期重新解析，避免卡在过期世界
static CACHED_AGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 纯内存读：从 startGame 的各容器合并角色（不碰 il2cpp API）。
fn startgame_roles(sg: usize) -> (Vec<usize>, String) {
    let mut dbg = String::new();
    let mut roles: Vec<usize> = Vec::new();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for off in [0x50usize, 0x2C8, 0x2D8, 0x350] {
        let l = read::<usize>(sg + off).unwrap_or(0);
        if l < 0x10000 {
            continue;
        }
        let Some((items, size)) = read_list(l) else { continue };
        let mut cnt = 0usize;
        for o in read_array_items(items, size, 512) {
            if seen.insert(o) {
                roles.push(o);
                cnt += 1;
            }
        }
        if cnt > 0 {
            dbg.push_str(&format!("  list@0x{off:X}: {cnt} 个\n"));
        }
    }
    let l = read::<usize>(sg + 0x98).unwrap_or(0);
    if l >= 0x10000 {
        if let Some((items, size)) = read_list(l) {
            let mut cnt = 0usize;
            for net in read_array_items(items, size, 512) {
                if let Some(r) = read::<usize>(net + 0x68) {
                    if r > 0x10000 && seen.insert(r) {
                        roles.push(r);
                        cnt += 1;
                    }
                }
            }
            if cnt > 0 {
                dbg.push_str(&format!("  RoleNetList@0x98: {cnt} 个\n"));
            }
        }
    }
    for (off, elem_is_list) in [(0x60usize, false), (0x70usize, true)] {
        let d = read::<usize>(sg + off).unwrap_or(0);
        if d < 0x10000 {
            continue;
        }
        let entries = read::<usize>(d + 0x18).unwrap_or(0);
        let count = read::<i32>(d + 0x20).unwrap_or(0);
        if entries < 0x10000 || count <= 0 {
            continue;
        }
        let n = (count as usize).min(1024);
        let mut cnt = 0usize;
        for i in 0..n {
            let e = entries + 0x20 + i * 0x18;
            if read::<i32>(e).unwrap_or(-1) < 0 {
                continue;
            }
            let val = read::<usize>(e + 0x10).unwrap_or(0);
            if val < 0x10000 {
                continue;
            }
            if elem_is_list {
                if let Some((items, size)) = read_list(val) {
                    for r in read_array_items(items, size, 512) {
                        if r > 0x10000 && seen.insert(r) {
                            roles.push(r);
                            cnt += 1;
                        }
                    }
                }
            } else if seen.insert(val) {
                roles.push(val);
                cnt += 1;
            }
        }
        if cnt > 0 {
            dbg.push_str(&format!("  dict@0x{off:X}: {cnt} 个\n"));
        }
    }
    dbg.push_str(&format!("  合并后共 {} 个角色\n", roles.len()));
    (roles, dbg)
}

pub fn battle_roles_verbose() -> (Vec<usize>, String) {
    let mut dbg = String::new();
    // 快路径：用缓存的 startGame 直接读列表（纯内存读，不碰 il2cpp API）
    let cached = CACHED_SG.load(std::sync::atomic::Ordering::Relaxed);
    if cached >= 0x10000 {
        let (roles, d) = startgame_roles(cached);
        if !roles.is_empty() {
            let age = CACHED_AGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // 角色多 → 直接用；只有 1 个且缓存较旧 → 隔 10 次（约 4 秒）重解析一次
            if roles.len() > 1 || age < 10 {
                return (roles, d);
            }
        }
    }
    CACHED_AGE.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut world: Option<usize> = None;

    // ① GameWorldClientManager.MyGameWorld（静态偏移 0x8）
    if let Some(k) = il2cpp_find_class("", "GameWorldClientManager") {
        // 手工读静态字段（static_fields + 0x8），并校验它的类是 BattleWorld。
        // 不调 il2cpp_static_field_get_value —— 它会触发类初始化，切场景时容易崩游戏。
        let mut w = 0usize;
        for off in STATIC_FIELDS_CANDIDATES {
            let Some(sf) = read::<usize>(k + off) else { continue };
            if sf < 0x10000 {
                continue;
            }
            let Some(v) = read::<usize>(sf + 0x8) else { continue };
            if v < 0x10000 {
                continue;
            }
            let ok = matches!(read::<usize>(v), Some(bk) if il2cpp_class_get_name_api(bk).as_deref() == Some("BattleWorld"));
            if ok {
                w = v;
                dbg.push_str(&format!(
                    "  MyGameWorld(手工 static_fields@0x{off:X}+0x8) = 0x{v:012X}\n"
                ));
                break;
            }
        }
        if w <= 0x10000 {
            for off in STATIC_FIELDS_CANDIDATES {
                let Some(sf) = read::<usize>(k + off) else { continue };
                if sf < 0x10000 {
                    continue;
                }
                if let Some(v) = read::<usize>(sf + 0x8) {
                    if v > 0x10000 {
                        dbg.push_str(&format!(
                            "  [手工] static_fields@0x{off:X}+0x8 = 0x{v:012X}\n"
                        ));
                        w = v;
                        break;
                    }
                }
            }
        }
        if w > 0x10000 {
            world = Some(w);
            dbg.push_str(&format!("  MyGameWorld = 0x{w:012X}\n"));
        }
    }
    // ② 备选静态源
    if world.is_none() {
        for (cls, fld, off) in [
            ("RandomEventState", "gameWorld", 0x0usize),
            ("AceSdkManagerServer", "myGameWorld", 0x0usize),
        ] {
            let Some(k) = il2cpp_find_class("", cls) else { continue };
            let mut w = 0usize;
            if w <= 0x10000 {
                for o in STATIC_FIELDS_CANDIDATES {
                    let Some(sf) = read::<usize>(k + o) else { continue };
                    if sf < 0x10000 {
                        continue;
                    }
                    if let Some(v) = read::<usize>(sf + off) {
                        if v > 0x10000 {
                            w = v;
                            break;
                        }
                    }
                }
            }
            if w > 0x10000 {
                dbg.push_str(&format!("  {cls}.{fld} = 0x{w:012X}\n"));
                world = Some(w);
                break;
            }
        }
    }
    let Some(world) = world else {
        dbg.push_str("  取不到 BattleWorld（静态字段全失败）\n");
        return (Vec::new(), dbg);
    };

    // ③ BattleWorld.startGame @0x4C8
    let sg = read::<usize>(world + 0x4C8).unwrap_or(0);
    if sg < 0x10000 {
        dbg.push_str(&format!(
            "  BattleWorld=0x{world:012X}，但 startGame(+0x4C8)={sg:#X}\n"
        ));
        return (Vec::new(), dbg);
    }
    dbg.push_str(&format!("  BattleWorld=0x{world:012X} startGame=0x{sg:012X}\n"));
    CACHED_SG.store(sg, std::sync::atomic::Ordering::Relaxed);

    // ④ 合并多个角色列表：不同模式/不同阶段用不同列表，单一 RoleList 常常为空
    let mut roles: Vec<usize> = Vec::new();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    // List<BattleRoleLogic>
    for off in [0x50usize, 0x2C8, 0x2D8, 0x350] {
        let l = read::<usize>(sg + off).unwrap_or(0);
        if l < 0x10000 {
            continue;
        }
        let Some((items, size)) = read_list(l) else { continue };
        let mut cnt = 0usize;
        for o in read_array_items(items, size, 512) {
            if o > 0x10000 && seen.insert(o) {
                roles.push(o);
                cnt += 1;
            }
        }
        if cnt > 0 || off == 0x50 {
            dbg.push_str(&format!("  list@0x{off:X}: {cnt} 个\n"));
        }
    }
    // List<RoleNet> @0x98 → RoleNet.MyRole @0x68
    let l = read::<usize>(sg + 0x98).unwrap_or(0);
    if l >= 0x10000 {
        if let Some((items, size)) = read_list(l) {
            let mut cnt = 0usize;
            for net in read_array_items(items, size, 512) {
                if let Some(r) = read::<usize>(net + 0x68) {
                    if r > 0x10000 && seen.insert(r) {
                        roles.push(r);
                        cnt += 1;
                    }
                }
            }
            if cnt > 0 {
                dbg.push_str(&format!("  RoleNetList@0x98: {cnt} 个\n"));
            }
        }
    }
    // Dictionary<TKey,TValue>：0x60 = Dictionary<long,BattleRoleLogic>；0x70 = Dictionary<string,List<BattleRoleLogic>>
    for (off, elem_is_list) in [(0x60usize, false), (0x70usize, true)] {
        let d = read::<usize>(sg + off).unwrap_or(0);
        if d < 0x10000 {
            continue;
        }
        let entries = read::<usize>(d + 0x18).unwrap_or(0);
        let count = read::<i32>(d + 0x20).unwrap_or(0);
        if entries < 0x10000 || count <= 0 {
            continue;
        }
        let n = (count as usize).min(1024);
        let mut cnt = 0usize;
        for i in 0..n {
            let e = entries + 0x20 + i * 0x18;
            // 空闲项 hashCode = -1
            if read::<i32>(e).unwrap_or(-1) < 0 {
                continue;
            }
            let val = read::<usize>(e + 0x10).unwrap_or(0);
            if val < 0x10000 {
                continue;
            }
            if elem_is_list {
                if let Some((items, size)) = read_list(val) {
                    for r in read_array_items(items, size, 512) {
                        if r > 0x10000 && seen.insert(r) {
                            roles.push(r);
                            cnt += 1;
                        }
                    }
                }
            } else if seen.insert(val) {
                roles.push(val);
                cnt += 1;
            }
        }
        if cnt > 0 {
            dbg.push_str(&format!("  dict@0x{off:X}: {cnt} 个\n"));
        }
    }
    dbg.push_str(&format!("  合并后共 {} 个角色\n", roles.len()));
    (roles, dbg)
}

/// 只走“便宜”的静态字段链拿角色列表（不扫描、不打印）：先权威链，再 AuxiliaryAimingUtil。
/// 人机来源：StartGame.MyRoleAIMgr(0x298) → RoleAIManager.logicList(0x18) → List<RoleAILogic>。
/// 很多模式里人机根本不在 StartGame.RoleList，只存在于 AI 管理器里。
pub fn ai_logic_list(cap: usize) -> Vec<usize> {
    let sg = CACHED_SG.load(std::sync::atomic::Ordering::Relaxed);
    if sg < 0x10000 {
        return Vec::new();
    }
    let mgr = read::<usize>(sg + 0x298).unwrap_or(0);
    if mgr < 0x10000 {
        return Vec::new();
    }
    let list = read::<usize>(mgr + 0x18).unwrap_or(0);
    if list < 0x10000 {
        return Vec::new();
    }
    let Some((items, size)) = read_list(list) else {
        return Vec::new();
    };
    read_array_items(items, size, cap)
}

pub fn roles_static_once(cap: usize) -> Vec<usize> {
    let (r, _) = battle_roles_verbose();
    if !r.is_empty() {
        return r;
    }
    roles_from_static_list(cap).unwrap_or_default()
}

// ===================== 静态字段链：AuxiliaryAimingUtil.roleList =====================

/// 用官方 API `il2cpp_field_static_get_value` 直接取某个静态字段的指针值。
/// 这是最可靠的方式，完全不依赖 `static_fields` 的手工偏移。
/// 手工读某个类的静态字段（给定静态偏移），用 valid 校验；不调任何 il2cpp API。
pub fn manual_static_ptr(klass: usize, static_off: usize, valid: impl Fn(usize) -> bool) -> Option<usize> {
    for off in STATIC_FIELDS_CANDIDATES {
        let Some(sf) = read::<usize>(klass + off) else { continue };
        if sf < 0x10000 {
            continue;
        }
        let Some(v) = read::<usize>(sf + static_off) else { continue };
        if v < 0x10000 {
            continue;
        }
        if valid(v) {
            return Some(v);
        }
    }
    None
}

/// 判断一个地址是不是 `List<T>` 对象：首字段是类指针，类名以 "List" 开头。
pub fn is_list_object(addr: usize) -> bool {
    if addr < 0x10000 {
        return false;
    }
    let Some(k) = read::<usize>(addr) else {
        return false;
    };
    il2cpp_class_get_name_api(k)
        .map(|n| n.starts_with("List"))
        .unwrap_or(false)
}

/// 读 `List<T>` 的 (`_items` 数组指针, `_size`)。IL2CPP 布局：`_items@0x10`、`_size@0x18`。
pub fn read_list(list: usize) -> Option<(usize, i32)> {
    let items = read::<usize>(list + 0x10)?;
    let size = read::<i32>(list + 0x18)?;
    if items < 0x10000 || !(-1..=4096).contains(&size) {
        return None;
    }
    Some((items, size))
}

/// IL2CPP 数组数据从 `+0x20` 开始，每 8 字节一个对象指针。取前 `min(size, cap)` 个。
pub fn read_array_items(items: usize, size: i32, cap: usize) -> Vec<usize> {
    let mut v = Vec::new();
    if items < 0x10000 || size <= 0 {
        return v;
    }
    let n = (size as usize).min(cap);
    for i in 0..n {
        if let Some(o) = read::<usize>(items + 0x20 + i * 8) {
            if o > 0x10000 {
                v.push(o);
            }
        }
    }
    v
}

/// 候选的 `Il2CppClass.static_fields` 偏移。
/// 按 il2cpp.h 精算：`Il2CppClass_1` 含两个 16 字节的 `Il2CppType`（byval_arg/this_arg），
/// 末尾 `interfaceOffsets` 落在 `0xB0`，故 `static_fields @ 0xB8`。邻近值作兜底。
pub const STATIC_FIELDS_CANDIDATES: [usize; 8] = [0xB8, 0xB0, 0xC0, 0xA8, 0x98, 0x90, 0xD0, 0xE0];

/// 走静态字段链拿 `AuxiliaryAimingUtil.roleList`（`List<...>` 对象指针），并返回诊断文本。
/// ① 先试官方 API `il2cpp_field_static_get_value`；② 再试手工 `static_fields + 0x8`。
pub fn aux_role_list_verbose() -> (Option<usize>, String) {
    let mut dbg = String::new();
    let Some(klass) = il2cpp_find_class("", "AuxiliaryAimingUtil") else {
        dbg.push_str("  找类 AuxiliaryAimingUtil 失败（API 不可用或名字不对）\n");
        return (None, dbg);
    };
    let kn = il2cpp_class_get_name_api(klass).unwrap_or_else(|| "?".into());
    dbg.push_str(&format!("  类 {kn} = 0x{klass:012X}\n"));
    // ① 手工读 roleList（static_fields + 0x8），校验是 List；不调 il2cpp API
    if let Some(p) = manual_static_ptr(klass, 0x8, |v| is_list_object(v)) {
        dbg.push_str(&format!("  [手工] roleList = 0x{p:012X}\n"));
        return (Some(p), dbg);
    }
    dbg.push_str("  [手工] roleList 取不到，转候选偏移链\n");
    // ② 手工 static_fields 链（候选偏移）
    for off in STATIC_FIELDS_CANDIDATES {
        let Some(sf) = read::<usize>(klass + off) else {
            continue;
        };
        if sf < 0x10000 {
            continue;
        }
        let Some(list) = read::<usize>(sf + 0x8) else {
            continue;
        };
        if list < 0x10000 {
            continue;
        }
        let ok = is_list_object(list);
        dbg.push_str(&format!(
            "  [手工] static_fields@0x{off:X}=0x{sf:012X} roleList=0x{list:012X} isList={ok}\n"
        ));
        if ok {
            return (Some(list), dbg);
        }
    }
    (None, dbg)
}

/// 全内存扫描真正的 BattleRoleLogic 实例：用“RoleNet(+0x740)→mirror(+0x90)→SyncHp(+0x168)
/// + 坐标(+0x7F8)”结构校验，过滤掉元数据表项。用于权威链失效时兜底。
pub fn scan_roles_by_structure(cap: usize) -> Vec<usize> {
    let Some(klass) = il2cpp_find_class("", "BattleRoleLogic") else {
        return Vec::new();
    };
    let hits = scan_objects_by_class(klass, cap);
    let mut v: Vec<usize> = Vec::new();
    for a in hits {
        if a < 0x10000 {
            continue;
        }
        let Some(net) = read::<usize>(a + 0x740) else { continue };
        if net < 0x10000 {
            continue;
        }
        let Some(mirror) = read::<usize>(net + 0x90) else { continue };
        if mirror < 0x10000 {
            continue;
        }
        let Some(hp) = read::<f32>(mirror + 0x168) else { continue };
        if !(hp.is_finite() && (-1.0..=1000.0).contains(&hp)) {
            continue;
        }
        let Some(p) = read_vec3_contig(a + 0x7F8) else { continue };
        if !(p.x.is_finite() && p.z.is_finite()) {
            continue;
        }
        if p.x.abs() > 20000.0 || p.z.abs() > 20000.0 {
            continue;
        }
        v.push(a);
    }
    v.sort_unstable();
    v.dedup();
    v
}

/// 走静态字段链，返回全部角色对象指针；失败返回 None。
#[allow(dead_code)]
pub fn roles_from_static_list(cap: usize) -> Option<Vec<usize>> {
    let (list, _dbg) = aux_role_list_verbose();
    let list = list?;
    let (items, size) = read_list(list)?;
    if size <= 0 {
        return None;
    }
    Some(read_array_items(items, size, cap))
}

/// 扫出所有“对象第一个字段 == 类指针”的对象地址。
pub fn scan_objects_by_class(klass: usize, cap: usize) -> Vec<usize> {
    scan_u64_in_range(klass as u64, klass as u64, cap)
        .into_iter()
        .map(|(a, _)| a)
        .collect()
}

/// 读一个 Il2CppClass 的类名（名字指针在 klass+0x10，可打印 ASCII）。
#[allow(dead_code)]
pub fn read_klass_name(klass: usize) -> Option<String> {
    let np = read::<usize>(klass + 0x10)?;
    if np < 0x10000 {
        return None;
    }
    let mut s: Vec<u8> = Vec::new();
    for i in 0..64usize {
        let b = read::<u8>(np + i)?;
        if b == 0 {
            break;
        }
        if !(b.is_ascii_graphic() || b == b'_') {
            return None;
        }
        s.push(b);
    }
    if s.len() < 2 {
        return None;
    }
    String::from_utf8(s).ok()
}

/// 扫出所有“类名 == target”的 IL2CPP 对象地址（*(obj) 是类指针，类名在 klass+0x10）。
#[allow(dead_code)]
pub fn scan_objects_by_class_name(target: &str, cap: usize) -> Vec<usize> {
    use std::collections::HashMap;
    let mut cache: HashMap<usize, bool> = HashMap::new();
    let mut hits: Vec<usize> = Vec::new();
    for_each_region_chunk(|addr, b| {
        if hits.len() >= cap {
            return;
        }
        let n = b.len() / 8;
        for i in 0..n {
            let off = i * 8;
            let q = u64::from_le_bytes([
                b[off],
                b[off + 1],
                b[off + 2],
                b[off + 3],
                b[off + 4],
                b[off + 5],
                b[off + 6],
                b[off + 7],
            ]);
            // 类指针一定在模块区
            if !(0x7F00_0000_0000..0x8000_0000_0000).contains(&q) {
                continue;
            }
            let k = q as usize;
            let ok = *cache
                .entry(k)
                .or_insert_with(|| read_klass_name(k).map(|n2| n2 == target).unwrap_or(false));
            if ok {
                hits.push(addr + off);
                if hits.len() >= cap {
                    return;
                }
            }
        }
    });
    hits
}

pub fn find_game_window() -> Option<HWND> {
    if let Some(hwnd) = find_process_hwnd() {
        return Some(hwnd.0 as HWND);
    }
    let name = s!("SauSage Man");
    //let name = s!("Counter-Strike: Global Offensive - Direct3D 9");
    let hwnd = unsafe { FindWindowA(std::ptr::null(), name) };

    if hwnd.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

#[allow(dead_code)]
static GAME_HWND: OnceLock<isize> = OnceLock::new();

/// （备用）游戏主窗口句柄。客户区尺寸现在走“枚举本进程可见窗口取最大客户区”，
/// 所以这里暂时没被调用，留着以后需要精确定位某个窗口时用。
#[allow(dead_code)]
pub fn game_hwnd() -> Option<HWND> {
    if let Some(&raw) = GAME_HWND.get() {
        let hwnd = raw as HWND;
        if unsafe { IsWindow(hwnd) } != 0 {
            return Some(hwnd);
        }
    };

    let hwnd = find_game_window()?;
    let _ = GAME_HWND.set(hwnd as isize);
    Some(hwnd)
}

/// 枚举回调：在进程自己的可见窗口里挑客户区面积最大的那个，记到 lparam。
unsafe extern "system" fn enum_win(
    hwnd: HWND,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> i32 {
    unsafe {
        let best = &mut *(lparam as *mut (i64, f32, f32));
        if windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut pid: u32 = 0;
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != windows_sys::Win32::System::Threading::GetCurrentProcessId() {
            return 1;
        }
        let mut r: RECT = std::mem::zeroed();
        if GetClientRect(hwnd, &mut r) == 0 {
            return 1;
        }
        let w = (r.right - r.left) as f32;
        let h = (r.bottom - r.top) as f32;
        if w > 0.0 && h > 0.0 {
            let area = (w * h) as i64;
            if area > best.0 {
                *best = (area, w, h);
            }
        }
    }
    1
}

/// 游戏实际可见像素 = 进程内“最大的可见窗口”的客户区。
/// 故意不用缓存 hwnd：加载期可能先出现一个临时窗口，把缓存的 hwnd 抓错，
/// 导致 imgui 的 display_size 变成错的（曾经就是 1902x970 vs 真实 1366x768）。
static CLIENT_CACHE: OnceLock<std::sync::Mutex<Option<(std::time::Instant, (f32, f32))>>> =
    OnceLock::new();

pub fn game_client_size() -> Option<(f32, f32)> {
    let cache = CLIENT_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(g) = cache.lock() {
        if let Some((t, s)) = *g {
            if t.elapsed().as_millis() < 1000 {
                return Some(s);
            }
        }
    }

    let mut best: (i64, f32, f32) = (0, 0.0, 0.0);
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows(
            Some(enum_win),
            &mut best as *mut _ as windows_sys::Win32::Foundation::LPARAM,
        );
    }
    if best.1 <= 0.0 || best.2 <= 0.0 {
        return None;
    }
    let s = (best.1, best.2);
    if let Ok(mut g) = cache.lock() {
        *g = Some((std::time::Instant::now(), s));
    }
    Some(s)
}
