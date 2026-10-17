// F21/CR03：真实、不可解析的私有 GUID name-surrogate reparse 输出目录项。
// 它不是 file-symlink；覆盖同一 exists() 误判/未经确认替换边界，不声称验证 symlink 特例。
// 只允许在调用方拥有的 tempfile/.tmp 中使用；create_new 防止改动任何已有目录项。
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT,
    },
    System::IO::DeviceIoControl,
};

const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00a4;
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00a8;
const PRIVATE_NAME_SURROGATE_TAG: u32 = 0x2000_0042;
const PRIVATE_GUID: [u8; 16] = [
    0x18, 0x3e, 0xca, 0x84, 0x9b, 0xe8, 0x46, 0x42, 0x90, 0x34, 0xf2, 0x17, 0x27, 0x26, 0x68,
    0x1c,
];

/// 目录项本身（不跟随 reparse）的原始状态；身份也必须不变，不能删除后重建伪装保留。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntrySnapshot {
    attributes: u32,
    volume_serial: u32,
    file_index: u64,
    creation_time: u64,
    last_write_time: u64,
    file_size: u64,
    number_of_links: u32,
    reparse_data: Vec<u8>,
}

fn guid_buffer() -> [u8; 24] {
    // REPARSE_GUID_DATA_BUFFER：tag(u32)、dataLength(u16)=0、reserved(u16)=0、GUID(16)。
    let mut data = [0u8; 24];
    data[..4].copy_from_slice(&PRIVATE_NAME_SURROGATE_TAG.to_le_bytes());
    data[8..].copy_from_slice(&PRIVATE_GUID);
    data
}

fn entry_snapshot(file: &File, path: &Path) -> EntrySnapshot {
    // SAFETY: BY_HANDLE_FILE_INFORMATION 是 POD，零初始化合法；同步 API 不保留指针。
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: file 在调用期间保持有效；info 是完整可写的输出结构。
    let success = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) };
    assert_ne!(
        success,
        0,
        "读取目录项身份失败（{}）：{}",
        path.display(),
        io::Error::last_os_error()
    );
    let reparse_data = if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        let mut data = [0u8; 16 * 1024];
        let mut returned = 0u32;
        // SAFETY: 有效 OPEN_REPARSE_POINT 句柄、完整输出缓冲区；同步调用不保留指针。
        let success = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_GET_REPARSE_POINT,
                std::ptr::null(),
                0,
                data.as_mut_ptr().cast(),
                u32::try_from(data.len()).unwrap(),
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            success,
            0,
            "读取真实 reparse 数据失败（{}）：{}",
            path.display(),
            io::Error::last_os_error()
        );
        let returned = usize::try_from(returned).unwrap();
        assert!((8..=data.len()).contains(&returned), "非法 reparse 返回长度：{returned}");
        data[..returned].to_vec()
    } else {
        Vec::new()
    };
    EntrySnapshot {
        attributes: info.dwFileAttributes,
        volume_serial: info.dwVolumeSerialNumber,
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        creation_time: (u64::from(info.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(info.ftCreationTime.dwLowDateTime),
        last_write_time: (u64::from(info.ftLastWriteTime.dwHighDateTime) << 32)
            | u64::from(info.ftLastWriteTime.dwLowDateTime),
        file_size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        number_of_links: info.nNumberOfLinks,
        reparse_data,
    }
}

/// 重新读取目录项本身；普通文件也返回快照，让未授权替换失败落在消费者的保护断言。
pub(crate) fn snapshot(path: &Path) -> EntrySnapshot {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap_or_else(|error| panic!("打开输出目录项失败（{}）：{error}", path.display()));
    entry_snapshot(&file, path)
}

/// 无提权创建真实不可解析输出目录项；任何夹具失败均直接报错，绝不回退或跳过。
pub(crate) fn create(path: &Path) -> EntrySnapshot {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap_or_else(|error| panic!("独占创建输出夹具失败（{}）：{error}", path.display()));
    let data = guid_buffer();
    let mut returned = 0u32;
    // SAFETY: 有效且独占新建的文件句柄；输入为完整 GUID reparse buffer，同步调用。
    let success = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            data.as_ptr().cast(),
            u32::try_from(data.len()).unwrap(),
            std::ptr::null_mut(),
            0,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        success,
        0,
        "设置真实 name-surrogate reparse 夹具失败（{}）：{}；不得回退普通文件或跳过",
        path.display(),
        io::Error::last_os_error()
    );
    // 先关闭唯一写句柄，避免关闭时提交的时间戳影响消费者前后快照比较。
    drop(file);
    let before = snapshot(path);
    assert_eq!(before.reparse_data, data, "GET 必须逐字节回读实际 SET 的 24 字节 GUID 数据");
    assert_ne!(before.attributes & FILE_ATTRIBUTE_REPARSE_POINT, 0);
    assert_eq!(before.file_size, 0);

    use std::os::windows::fs::MetadataExt;
    let entry = fs::symlink_metadata(path).unwrap();
    assert_ne!(entry.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT, 0);
    let error = fs::metadata(path).expect_err("跟随 name-surrogate 目录项必须失败");
    assert_eq!(error.raw_os_error(), Some(1920), "应为 ERROR_CANT_ACCESS_FILE：{error}");
    assert!(!path.exists(), "必须真实触发 exists() 的误判边界");
    assert_eq!(snapshot(path), before, "夹具校验不得改变目录项");
    before
}

/// 明确授权后的正控：必须替换为正确普通输出，不能保留 reparse 或只写底层流。
pub(crate) fn assert_regular_output(path: &Path, expected: &[u8]) {
    use std::os::windows::fs::MetadataExt;
    let entry = fs::symlink_metadata(path).unwrap();
    assert_eq!(entry.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT, 0);
    assert!(entry.is_file());
    assert!(!entry.file_type().is_symlink());
    assert_eq!(fs::read(path).unwrap(), expected);
}
