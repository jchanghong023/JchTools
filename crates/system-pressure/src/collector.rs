use crate::window::{Disk, Sample};
use chrono::Utc;
use std::collections::BTreeMap;
use std::ptr;
use windows_sys::Win32::System::{
    Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhGetFormattedCounterValue, PdhGetRawCounterArrayW, PdhOpenQueryW, PDH_CSTATUS_NEW_DATA,
        PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
        PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PDH_RAW_COUNTER_ITEM_W,
    },
    SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
};

// windows-sys 0.61未生成这两个SDK宏；数值来自微软Pdh.h，不另声明系统ABI。
// NOSCALE保持Bytes/sec原始单位，NOCAP100禁止把高速吞吐封顶到100。
const PDH_FMT_NOSCALE: u32 = 0x1000;
const PDH_FMT_NOCAP100: u32 = 0x8000;
const FORMAT: u32 = PDH_FMT_DOUBLE | PDH_FMT_NOSCALE | PDH_FMT_NOCAP100;

pub(crate) struct Collector {
    query: PDH_HQUERY,
    cpu: PDH_HCOUNTER,
    idle: PDH_HCOUNTER,
    read: PDH_HCOUNTER,
    write: PDH_HCOUNTER,
    primed: bool,
    formatted: Vec<PDH_FMT_COUNTERVALUE_ITEM_W>,
    raw: Vec<PDH_RAW_COUNTER_ITEM_W>,
    known_disks: Vec<u32>,
}

impl Collector {
    pub(crate) fn new() -> Self {
        let mut collector = Self {
            query: ptr::null_mut(),
            cpu: ptr::null_mut(),
            idle: ptr::null_mut(),
            read: ptr::null_mut(),
            write: ptr::null_mut(),
            primed: false,
            formatted: Vec::new(),
            raw: Vec::new(),
            known_disks: Vec::new(),
        };
        // SAFETY: 空数据源表示本机实时查询，输出句柄指向有效内存。
        let result = unsafe { PdhOpenQueryW(ptr::null(), 0, &raw mut collector.query) };
        if result != 0 {
            tracing::warn!(
                event = "system_pressure_counter_unavailable",
                code = result,
                "系统性能查询不可用，缺失指标记录为 N/A"
            );
            collector.query = ptr::null_mut();
            return collector;
        }
        // Processor Information 的 _Total 包括所有处理器组，不局限于单个组的64个逻辑CPU。
        collector.cpu = collector.counter(
            "整机CPU",
            windows_sys::w!(r"\Processor Information(_Total)\% Processor Time"),
        );
        // PhysicalDisk 实例对应物理盘，不用 LogicalDisk 分区或磁盘空间替代 I/O 压力。
        // 100 - Idle Time 表示实际繁忙时间，避免队列累计的 % Disk Time 超过100%。
        collector.idle = collector.counter(
            "物理磁盘空闲时间",
            windows_sys::w!(r"\PhysicalDisk(*)\% Idle Time"),
        );
        collector.read = collector.counter(
            "物理磁盘读取速度",
            windows_sys::w!(r"\PhysicalDisk(*)\Disk Read Bytes/sec"),
        );
        collector.write = collector.counter(
            "物理磁盘写入速度",
            windows_sys::w!(r"\PhysicalDisk(*)\Disk Write Bytes/sec"),
        );
        collector
    }

    fn counter(&self, name: &str, path: *const u16) -> PDH_HCOUNTER {
        let mut counter = ptr::null_mut();
        // SAFETY: 查询句柄有效；英文路径以NUL结尾，输出句柄有效。PDH负责本地化。
        let result = unsafe { PdhAddEnglishCounterW(self.query, path, 0, &raw mut counter) };
        if result != 0 {
            tracing::warn!(
                event = "system_pressure_counter_unavailable",
                counter = name,
                code = result,
                "系统性能计数器不可用，缺失指标记录为 N/A"
            );
            return ptr::null_mut();
        }
        counter
    }

    pub(crate) fn sample(&mut self) -> Sample {
        // SAFETY: 非空句柄由本Collector创建并持有至Drop。
        let collected = !self.query.is_null() && unsafe { PdhCollectQueryData(self.query) } == 0;
        let interval_available = self.primed && collected;
        self.primed = collected;
        let mut disks = BTreeMap::new();
        if collected {
            if interval_available {
                for (counter, metric) in [(self.idle, 0), (self.read, 1), (self.write, 2)] {
                    self.disk_values(counter, metric, &mut disks);
                }
            }
            if disks.is_empty() {
                for counter in [self.idle, self.read, self.write] {
                    self.disk_names(counter, &mut disks);
                    if !disks.is_empty() {
                        break;
                    }
                }
            }
        }
        if disks.is_empty() {
            for &number in &self.known_disks {
                disks.insert(
                    number,
                    Disk {
                        number,
                        ..Disk::default()
                    },
                );
            }
        } else {
            self.known_disks.clear();
            self.known_disks.extend(disks.keys().copied());
        }
        Sample {
            at: Utc::now(),
            cpu: if interval_available {
                self.cpu_value()
            } else {
                None
            },
            memory: memory_percent(),
            disks: disks.into_values().collect(),
        }
    }

    fn cpu_value(&self) -> Option<f64> {
        if self.cpu.is_null() {
            return None;
        }
        let mut value = PDH_FMT_COUNTERVALUE::default();
        // SAFETY: CPU句柄属于当前查询，value是有效且对齐的输出结构。
        let result = unsafe {
            PdhGetFormattedCounterValue(self.cpu, FORMAT, ptr::null_mut(), &raw mut value)
        };
        if result != 0 {
            return None;
        }
        numeric(value).map(|value| value.clamp(0.0, 100.0))
    }

    fn disk_names(&mut self, counter: PDH_HCOUNTER, disks: &mut BTreeMap<u32, Disk>) {
        if counter.is_null() {
            return;
        }
        let mut bytes = 0;
        let mut count = 0;
        // SAFETY: 空缓冲区请求所需大小；计数器有效，大小与数量输出有效。
        let result = unsafe {
            PdhGetRawCounterArrayW(counter, &raw mut bytes, &raw mut count, ptr::null_mut())
        };
        if result != PDH_MORE_DATA || bytes == 0 {
            return;
        }
        let Ok(size) = usize::try_from(bytes) else {
            return;
        };
        self.raw.resize(
            size.div_ceil(std::mem::size_of::<PDH_RAW_COUNTER_ITEM_W>()),
            PDH_RAW_COUNTER_ITEM_W::default(),
        );
        // SAFETY: 缓冲区按官方结构对齐并至少有bytes字节，PDH在其中提供实例与名称。
        let result = unsafe {
            PdhGetRawCounterArrayW(
                counter,
                &raw mut bytes,
                &raw mut count,
                self.raw.as_mut_ptr(),
            )
        };
        if result != 0 {
            return;
        }
        for item in self.raw.iter().take(usize::try_from(count).unwrap_or(0)) {
            if let Some(number) = disk_number(item.szName) {
                disks.entry(number).or_insert_with(|| Disk {
                    number,
                    ..Disk::default()
                });
            }
        }
    }

    fn disk_values(&mut self, counter: PDH_HCOUNTER, metric: u8, disks: &mut BTreeMap<u32, Disk>) {
        if counter.is_null() {
            return;
        }
        let mut bytes = 0;
        let mut count = 0;
        // SAFETY: 空缓冲区请求所需大小；计数器有效，大小与数量输出有效。
        let result = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                FORMAT,
                &raw mut bytes,
                &raw mut count,
                ptr::null_mut(),
            )
        };
        if result != PDH_MORE_DATA || bytes == 0 {
            return;
        }
        let Ok(size) = usize::try_from(bytes) else {
            return;
        };
        self.formatted.resize(
            size.div_ceil(std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()),
            PDH_FMT_COUNTERVALUE_ITEM_W::default(),
        );
        // SAFETY: 缓冲区按官方结构对齐并至少有bytes字节，PDH输出数量及项均在其中。
        let result = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                FORMAT,
                &raw mut bytes,
                &raw mut count,
                self.formatted.as_mut_ptr(),
            )
        };
        if result != 0 {
            return;
        }
        for item in self
            .formatted
            .iter()
            .take(usize::try_from(count).unwrap_or(0))
        {
            if let Some(number) = disk_number(item.szName) {
                let disk = disks.entry(number).or_insert_with(|| Disk {
                    number,
                    ..Disk::default()
                });
                let value = numeric(item.FmtValue);
                match metric {
                    0 => disk.busy = value.map(|idle| (100.0 - idle).clamp(0.0, 100.0)),
                    1 => disk.read = value.map(|bytes| bytes / 1_048_576.0),
                    _ => disk.write = value.map(|bytes| bytes / 1_048_576.0),
                }
            }
        }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        if !self.query.is_null() {
            // SAFETY: 查询仅由本Collector拥有，关闭时一并释放所属计数器。
            unsafe { PdhCloseQuery(self.query) };
        }
    }
}

fn numeric(value: PDH_FMT_COUNTERVALUE) -> Option<f64> {
    if value.CStatus != PDH_CSTATUS_VALID_DATA && value.CStatus != PDH_CSTATUS_NEW_DATA {
        return None;
    }
    // SAFETY: 所有格式化查询都请求PDH_FMT_DOUBLE，成功状态保证union对应doubleValue。
    let number = unsafe { value.Anonymous.doubleValue };
    (number.is_finite() && number >= 0.0).then_some(number)
}

fn disk_number(name: *const u16) -> Option<u32> {
    if name.is_null() {
        return None;
    }
    let mut number = 0_u32;
    let mut offset = 0;
    loop {
        // SAFETY: 名称来自仍存活的PDH输出缓冲区，是NUL结尾的UTF-16；仅扫描数字前缀。
        let current = unsafe { name.add(offset) };
        // SAFETY: current 指向上述缓冲区中本次正在扫描的有效字符。
        let character = unsafe { *current };
        if !(u16::from(b'0')..=u16::from(b'9')).contains(&character) {
            return (offset > 0 && (character == 0 || character == u16::from(b' ')))
                .then_some(number);
        }
        number = number
            .checked_mul(10)?
            .checked_add(u32::from(character - u16::from(b'0')))?;
        offset += 1;
    }
}

fn memory_percent() -> Option<f64> {
    let mut memory = MEMORYSTATUSEX {
        dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>()).ok()?,
        ..MEMORYSTATUSEX::default()
    };
    // SAFETY: 结构长度正确，整个结构可写且存活至调用结束。
    if unsafe { GlobalMemoryStatusEx(&raw mut memory) } == 0 || memory.ullTotalPhys == 0 {
        return None;
    }
    // Windows直接报告物理内存已用百分比，不混入提交内存或页文件。
    Some(f64::from(memory.dwMemoryLoad))
}
