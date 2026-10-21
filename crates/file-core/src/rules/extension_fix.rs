/// C-08 / 附录 B：按内容签名修正错误扩展名的保守判定。true = 旧扩展名确实错误、可以改名。
/// 只有有效结构与必要类型标识确认的承诺类型才作为目标；旧后缀未知或为空
/// 不会否决可靠判型。只识别到 ZIP 外层时仍保护专用容器后缀，复合/分卷后缀
/// 和合法同义写法也始终保留。
pub fn extension_needs_fix(old: &str, detected: &str) -> bool {
    !equivalent_extension(old, detected)
        && !unsplittable_suffix(old)
        && !coarser_detection(old, detected)
        && correctable_type(detected)
}
/// 同义扩展名（同一格式的常见写法），不算错误扩展名。
fn equivalent_extension(old: &str, detected: &str) -> bool {
    old == detected
        || (matches!(old, "jpeg" | "jpe" | "jfif") && detected == "jpg")
        || (old == "tiff" && detected == "tif")
        || (old == "htm" && detected == "html")
        || (old == "mid" && detected == "midi")
}
/// 附录 B 552-557：只有这些类型是「可确定识别并修正」的改名目标；
/// 未承诺的类型不进行修正，不以模糊特征强行分类。
/// ZIP 内部类型由 signature 模块的有效 MIME 标识确认，包括宏文档与 ODF/EPUB。
fn correctable_type(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "gif"
            | "bmp"
            | "tif"
            | "webp"
            | "pdf"
            | "flac"
            | "wav"
            | "zip"
            | "7z"
            | "rar"
            | "docx"
            | "xlsx"
            | "pptx"
            | "docm"
            | "xlsm"
            | "pptm"
            | "odt"
            | "ods"
            | "odp"
            | "epub"
    )
}
/// 附录 B 531-532：复合扩展名（`.tar.gz` 等）与 X-10 卷尾/编号后缀（`.NNN`、`.rNN`、
/// `.zNN`、`.partN`）在改名时整体保留、不拆不改；改名目标都是单段扩展名，
/// 因此凡是不止一段的后缀都不作为「错误扩展名」处理。
fn unsplittable_suffix(ext: &str) -> bool {
    if ext.contains('.') {
        return true;
    }
    if ext.len() == 3 && ext.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if let Some(rest) = ext.strip_prefix(['r', 'z']) {
        return rest.len() == 2 && rest.bytes().all(|b| b.is_ascii_digit());
    }
    if let Some(rest) = ext.strip_prefix("part") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    false
}
/// 只确认 ZIP 外层、无法确认专用容器内型时，不能把文档或程序包改粗。
fn coarser_detection(old: &str, detected: &str) -> bool {
    detected == "zip"
        && (office_extension(old) || legacy_office_extension(old) || container_extension(old))
}
/// 旧式 Office 后缀（OLE 文档）：内容识别为 ZIP 容器时它已不是旧式文档，但也无法确定
/// 是不是伪装成该后缀的 OOXML，按 C-08 不猜测、不降级为压缩包后缀。
fn legacy_office_extension(ext: &str) -> bool {
    matches!(ext, "doc" | "xls" | "ppt")
}
/// Office（OOXML 基础类型与模板/宏变体）。
fn office_extension(ext: &str) -> bool {
    matches!(
        ext,
        "docx"
            | "docm"
            | "dotx"
            | "dotm"
            | "xlsx"
            | "xlsm"
            | "xltx"
            | "xltm"
            | "xlsb"
            | "pptx"
            | "pptm"
            | "potx"
            | "potm"
            | "ppsx"
            | "ppsm"
    )
}
/// ODF、EPUB 与 X-09 其他已列容器格式：识别结果只有外层 ZIP 容器时保留原后缀。
fn container_extension(ext: &str) -> bool {
    matches!(
        ext,
        "odt"
            | "ods"
            | "odp"
            | "epub"
            | "odg"
            | "odf"
            | "xps"
            | "vsdx"
            | "sldx"
            | "thmx"
            | "cbz"
            | "kmz"
            | "sxw"
            | "ott"
            | "ots"
            | "otp"
            | "odm"
            | "cab"
            | "msi"
            | "msix"
            | "appx"
            | "msixbundle"
            | "appxbundle"
            | "iso"
            | "img"
            | "wim"
            | "esd"
            | "vhd"
            | "vhdx"
            | "jar"
            | "war"
            | "ear"
            | "apk"
            | "aab"
            | "whl"
            | "nupkg"
            | "vsix"
            | "crx"
            | "xpi"
            | "ipa"
            | "pkg"
            | "dmg"
            | "lz4"
            | "br"
            | "lz"
            | "exe"
            | "com"
    )
}
