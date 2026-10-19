//! Windows 性能计数器（PDH）的最小封装。
//!
//! 用来在**非 NVIDIA** 显卡上兜底获取显存占用与 GPU 利用率：Windows 10 1803 起
//! 系统自带 `GPU Engine` 与 `GPU Adapter Memory` 计数器组，与厂商无关。
//!
//! 只用 PDH 英文计数器名（`pdh.dll` 在非英文系统上同样接受英文名），不依赖
//! WMI / DXGI，也不新增第三方 crate，符合「少一个依赖」的取舍。
//!
//! 通配路径（`(*)`）在本进程里拿不到实例，所以流程固定为两步：先用
//! [`enumerate_instances`] 枚举实例名，再用 [`PdhQuery`] 逐实例查询精确路径。
//! PDH 句柄不是 `Send`，只在后台采样线程里创建与使用，不要跨线程传递。

use std::ffi::c_void;

const MEM_COMMIT: u32 = 0x0000_1000;
const MEM_RESERVE: u32 = 0x0000_2000;
const MEM_RELEASE: u32 = 0x0000_8000;
const PAGE_READWRITE: u32 = 0x04;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualAlloc(
        address: *mut c_void,
        size: usize,
        allocation_type: u32,
        protect: u32,
    ) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, free_type: u32) -> i32;
}

const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
const PDH_FMT_NOSCALE: u32 = 0x0000_1000;
const PDH_MORE_DATA: u32 = 0x8000_07D2;
const ERROR_SUCCESS: u32 = 0;

/// PDH 格式化计数器值的接口结构体：`C` 的联合体（双精度 / 长整型 / 字符串指针）
/// 按 8 字节对齐，这里统一用 `f64` 承载，只读取双精度分支。
#[repr(C)]
#[derive(Clone, Copy)]
struct PdhFmtCounterValue {
    c_status: u32,
    double_value: f64,
}

#[link(name = "pdh")]
unsafe extern "system" {
    fn PdhOpenQueryW(data_source: *const u16, user_data: usize, query: *mut *mut c_void) -> u32;
    fn PdhAddEnglishCounterW(
        query: *mut c_void,
        counter_path: *const u16,
        user_data: usize,
        counter: *mut *mut c_void,
    ) -> u32;
    fn PdhCollectQueryData(query: *mut c_void) -> u32;
    fn PdhGetFormattedCounterValue(
        counter: *mut c_void,
        fmt: u32,
        counter_type: *mut u32,
        value: *mut PdhFmtCounterValue,
    ) -> u32;
    fn PdhCloseQuery(query: *mut c_void) -> u32;
    fn PdhEnumObjectItemsW(
        data_source: *const u16,
        machine_name: *const u16,
        object_name: *const u16,
        counter_list: *mut u16,
        counter_list_size: *mut u32,
        instance_list: *mut u16,
        instance_list_size: *mut u32,
        index: u32,
    ) -> u32;
}

/// 绑定单个**精确**计数器路径的查询（实例名不含 `(*)`）。
///
/// 用法：`open` 后连续 `collect()` 两次（速率类计数器需要一段采样区间），
/// 再用 `value()` 读取。
pub struct PdhQuery {
    handle: *mut c_void,
}

impl PdhQuery {
    /// 打开查询并绑定计数器；路径无效时返回 None。
    ///
    /// 路径形如 `r"\GPU Engine(pid_1_luid_...)\Utilization Percentage"`；
    /// 实例名先用 [`enumerate_instances`] 取。
    pub fn open(path: &str) -> Option<Self> {
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: 传入的指针均有效；句柄创建后由本结构体的 Drop 关闭。
        if unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut handle) } != ERROR_SUCCESS {
            return None;
        }
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut counter: *mut c_void = std::ptr::null_mut();
        // SAFETY: wide 以 NUL 结尾且在整个调用期间有效。
        let status = unsafe { PdhAddEnglishCounterW(handle, wide.as_ptr(), 0, &mut counter) };
        if status != ERROR_SUCCESS {
            // SAFETY: handle 有效且尚未交给外部，这里直接丢弃。
            let _ = unsafe { PdhCloseQuery(handle) };
            return None;
        }
        Some(Self { handle })
    }

    /// 采集一次数据：首次调用只建立基线，速率类计数器要在第二次采集后才有效。
    pub fn collect(&mut self) -> bool {
        // SAFETY: handle 由 PdhOpenQueryW 创建，且只在 Drop 中关闭一次。
        unsafe { PdhCollectQueryData(self.handle) == ERROR_SUCCESS }
    }

    /// 读取绑定计数器的当前值；没有数据或取数失败时返回 None。
    pub fn value(&self) -> Option<f64> {
        let mut value = PdhFmtCounterValue {
            c_status: 0,
            double_value: 0.0,
        };
        // SAFETY: self.handle 由 PdhOpenQueryW 创建；value 是有效可变指针。
        let status = unsafe {
            PdhGetFormattedCounterValue(
                self.handle,
                PDH_FMT_DOUBLE | PDH_FMT_NOSCALE,
                std::ptr::null_mut(),
                &mut value,
            )
        };
        // 部分驱动会给出轻微状态位（如 PDH_CSTATUS_INVALID_DATA），数值仍可用，
        // 因此只按取数返回码判断失败。
        (status == ERROR_SUCCESS).then_some(value.double_value)
    }
}
/// PDH 的枚举函数要求缓冲区可写可查询（in/out 参数）。
///
/// 不能用 `Vec`：VBS / CET 开启时 PDH 内部的 `VirtualQuery` 校验可能失效，
/// 直接写 Rust 分配的内存会触发访问违例。这里用系统堆分配并保留指针与长度。
struct QueryableBuffer {
    ptr: *mut u16,
    len: usize,
}

impl QueryableBuffer {
    /// 分配 `len` 个 u16（至少 2 个，保证双 NUL 结尾）。
    fn new(len: usize) -> Self {
        let len = len.max(2);
        let bytes = len * std::mem::size_of::<u16>();
        // SAFETY: 分配 bytes 字节的可读写内存；成功时指针有效。
        let ptr = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                bytes,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        assert!(!ptr.is_null(), "VirtualAlloc failed");
        Self {
            ptr: ptr.cast::<u16>(),
            len,
        }
    }

    fn as_mut_ptr(&mut self) -> *mut u16 {
        self.ptr
    }

    fn as_slice(&self, filled: usize) -> &[u16] {
        // SAFETY: 指针指向 len 个 u16，filled 不超过 len。
        unsafe { std::slice::from_raw_parts(self.ptr, filled.min(self.len)) }
    }
}

impl Drop for QueryableBuffer {
    fn drop(&mut self) {
        // SAFETY: 指针由 VirtualAlloc 分配且只释放一次。
        unsafe {
            let _ = VirtualFree(self.ptr.cast::<c_void>(), 0, MEM_RELEASE);
        }
    }
}

/// 枚举一个 PDH 对象（如 `GPU Engine`、`GPU Adapter Memory`）的全部实例名。
///
/// 返回的实例名可直接拼成精确计数器路径：`\\对象(实例)\\计数器`。
pub fn enumerate_instances(object: &str) -> Vec<String> {
    let obj: Vec<u16> = object.encode_utf16().chain(std::iter::once(0)).collect();
    let mut counter_len = 0u32;
    let mut instance_len = 0u32;
    // 两段式：第一次取长度（PDH 以 PDH_MORE_DATA 表示需要缓冲区）。
    // SAFETY: 传 null 缓冲区只取所需长度，obj 以 NUL 结尾。
    let status = unsafe {
        PdhEnumObjectItemsW(
            std::ptr::null(),
            std::ptr::null(),
            obj.as_ptr(),
            std::ptr::null_mut(),
            &mut counter_len,
            std::ptr::null_mut(),
            &mut instance_len,
            0,
        )
    };
    if status != PDH_MORE_DATA || instance_len == 0 {
        return Vec::new();
    }

    let mut counters = QueryableBuffer::new(counter_len as usize);
    let mut instances = QueryableBuffer::new(instance_len as usize);
    let mut counter_len2 = counter_len;
    let mut instance_len2 = instance_len;
    // SAFETY: 两个缓冲区长度均不小于第一步要求的长度。
    let status = unsafe {
        PdhEnumObjectItemsW(
            std::ptr::null(),
            std::ptr::null(),
            obj.as_ptr(),
            counters.as_mut_ptr(),
            &mut counter_len2,
            instances.as_mut_ptr(),
            &mut instance_len2,
            0,
        )
    };
    if status != ERROR_SUCCESS {
        return Vec::new();
    }
    // 实例列表是双 NUL 结尾的多字符串。
    let filled = (instance_len as usize).min(instance_len2 as usize + 2);
    instances
        .as_slice(filled)
        .split(|c| *c == 0)
        .filter(|name| !name.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// 计数器实例名去掉路径部分，只保留括号内的实例名。
///
/// PDH 返回的 `sz_name` 可能是实例名本身，也可能是完整计数器路径，两种情况都要处理。
fn instance_of(name: &str) -> &str {
    if let Some((_, rest)) = name.split_once('(') {
        return rest.split(')').next().unwrap_or("");
    }
    match (name.find('\\'), name.rfind('\\')) {
        (Some(first), Some(last)) if last > first => &name[last + 1..],
        _ => name,
    }
}

/// 从实例名里取出 `luid_...` 段（同一张卡的引擎与显存实例用同一个 LUID）。
fn luid_of(instance: &str) -> Option<String> {
    let start = instance.find("luid_")?;
    let rest = &instance[start..];
    let end = rest
        .find("_phys_")
        .map(|i| start + i)
        .unwrap_or(instance.len());
    Some(instance[start..end].to_string())
}

fn pid_of(instance: &str) -> Option<u32> {
    let rest = instance.strip_prefix("pid_")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 汇总 `GPU Engine(*)`：返回按 LUID 归并的利用率（每张卡一项）与涉及的进程 PID。
///
/// 利用率按任务管理器口径求和：同一张卡按引擎类型分别计数，合计即整卡占用。
/// `pid_4` 是系统合成进程、`pid_0` 是非进程上下文，不代表实际负载，统计时排除。
pub fn summarize_gpu_engines(values: &[(String, f64)]) -> (Vec<(String, f64)>, Vec<u32>) {
    let mut by_luid: Vec<(String, f64)> = Vec::new();
    let mut pids: Vec<u32> = Vec::new();
    for (raw, value) in values {
        let inst = instance_of(raw);
        let Some(luid) = luid_of(inst) else { continue };
        if let Some(pid) = pid_of(inst)
            && pid > 4
            && !pids.contains(&pid)
        {
            pids.push(pid);
        }
        match by_luid.iter_mut().find(|(key, _)| *key == luid) {
            Some((_, sum)) => *sum += value,
            None => by_luid.push((luid, *value)),
        }
    }
    (by_luid, pids)
}
