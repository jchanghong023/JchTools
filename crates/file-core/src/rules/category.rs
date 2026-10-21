use super::archive::numbered_volume_tail;

// ---------------------------------------------------------------------------
// 附录 A：普通文件大类映射（C-05 / C-06）
// ---------------------------------------------------------------------------

/// 附录 A 唯一归类映射：按最终完整文件名（小写）匹配，先匹配最长复合后缀，再匹配
/// 最后一段扩展名；X-10 命名族分卷（`.7z.001`、`.partN.rar`、`.rNN`、`.zNN`）按
/// 附录 A 说明归「压缩包」；未匹配与无扩展名归「其他」。
// 输入已预先小写，ends_with 的字面量比较即为忽略大小写口径。
// [quality-baseline approved 2026-10-03] 可证明误报（输入预小写），经用户裁定保留
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub fn category_for(name_lower: &str) -> &'static str {
    // 附录 B：首位点之后没有其他点的名称整体是主体，不存在扩展名。
    if name_lower.starts_with('.') && !name_lower[1..].contains('.') {
        return "其他";
    }
    const COMPOUND: [&str; 6] = [
        ".tar.gz",
        ".tar.bz2",
        ".tar.xz",
        ".tar.zst",
        ".tar.lzma",
        ".tar.z",
    ];
    if COMPOUND.iter().any(|suffix| name_lower.ends_with(suffix)) {
        return "压缩包";
    }
    // X-10 数字尾卷：白名单扩展名 + 规范编号（最少三位补零，`.999` 后自然增长）。
    if numbered_volume_tail(name_lower).is_some() {
        return "压缩包";
    }
    if name_lower.ends_with(".rar") {
        return "压缩包";
    }
    // 老式尾卷族：.r00～.r99 / .z01～.z99（附录 A：整理中的 X-10 命名族分卷归「压缩包」；
    // zip 族起始编号是 01，z00 不属命名族；data.001 这类无格式孤立编号仍归「其他」）。
    if let Some((_, tail)) = name_lower.rsplit_once('.') {
        let digits = tail.as_bytes();
        if tail.len() == 3
            && (tail.starts_with('r') || tail.starts_with('z'))
            && digits[1..].iter().all(u8::is_ascii_digit)
            && (tail.starts_with('r') || &tail[1..] != "00")
        {
            return "压缩包";
        }
    }
    let extension = name_lower.rsplit_once('.').map_or("", |(_, tail)| tail);
    match extension {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "ts"
        | "mts" | "m2ts" | "3gp" | "vob" | "rm" | "rmvb" => "视频",
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "opus" | "wma" | "aiff" | "aif"
        | "ape" | "mid" | "midi" => "音频",
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "svg" | "ico"
        | "heic" | "heif" | "avif" | "psd" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" => {
            "图片"
        }
        "pdf" | "txt" | "md" | "rtf" | "doc" | "docx" | "docm" | "xls" | "xlsx" | "xlsm"
        | "csv" | "ppt" | "pptx" | "pptm" | "odt" | "ods" | "odp" | "epub" | "mobi" | "azw"
        | "azw3" | "chm" | "html" | "htm" => "文档",
        "zip" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "lzma" | "z" | "tgz" | "tbz2"
        | "txz" | "tzst" | "lz4" | "br" | "lz" => "压缩包",
        "exe" | "com" | "msi" | "msix" | "appx" | "msixbundle" | "appxbundle" | "bat" | "cmd"
        | "ps1" | "apk" => "程序",
        _ => "其他",
    }
}
