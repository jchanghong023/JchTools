# CCITT Group 3 and 4 image encodings.

Currently supported:
- de- and encoding group 4 images
- decoding group 3 images


You can ask questions on [Zulip](https://type.zulipchat.com/#narrow/stream/209232-pdf/topic/fax.20.2F.20master)

本地修复：Group 4 水平游程与垂直转折越出图像宽度时返回解码错误，不把截断行视为成功；编码宽行时，垂直偏移使用足够宽的整数，首游程颜色判定不再对位置求和以避免溢出。回归位于 `src/decoder.rs` 的 `g4_horizontal_width_boundary`（含 TIFF 消费端）与 `src/encoder.rs` 的 `g4_large_width_roundtrip`。
