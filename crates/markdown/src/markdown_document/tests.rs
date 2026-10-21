    use super::*;
    use serde_json::json;

    fn response(document: &Value) -> Value {
        json!({"id": 1, "ok": true, "document": document, "warnings": []})
    }

    // 覆盖 T-14/T-17：content 是引擎最终 Markdown，零改写（不追加换行、不拼
    // children、不再规范化）；唯一调用方步骤是媒体目录前缀。children 的并入
    // 由引擎负责，适配器忽略。
    #[test]
    fn content_is_final_and_children_are_ignored() {
        let envelope = response(&json!({
            "content": "# 报告\n\n![图](image_0.png)\n",
            "images": [
                {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}
            ],
            "children": [
                {"path": "word/embeddings/a.docx", "mime_type": "x",
                 "result": {"content": "子文档正文", "mime_type": "x", "metadata": {}, "tables": []}}
            ]
        }));
        let output = parse_document_response(&envelope, "a_docx_media").unwrap();
        assert_eq!(
            output.markdown,
            "# 报告\n\n![图](a_docx_media/image_0.png)\n"
        );
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "a_docx_media/image_0.png");
        assert_eq!(output.media[0].bytes, b"hello".to_vec());
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14：媒体目录名按 CommonMark 链接目标编码，跨行行内代码引用保持原样。
    #[test]
    fn media_prefix_is_encoded_and_multiline_inline_code_is_untouched() {
        let envelope = response(&json!({
            "content": "before `literal\n![代码](image_0.png)` after\n![图](image_0.png)",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "a_(1)%#").unwrap();
        assert_eq!(
            output.markdown,
            "before `literal\n![代码](image_0.png)` after\n![图](a_%281%29%25%23/image_0.png)"
        );
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "a_(1)%#/image_0.png");
    }

    // 覆盖 T-14/T-17：转义图片标记和反引号都是正文字符，不产生图片或代码跨度。
    #[test]
    fn escaped_image_markers_remain_literal() {
        let envelope = response(&json!({
            "content": "\\![字面](image_0.png)\n\\` ![图](image_1.png)",
            "images": [
                {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="},
                {"image_index": 1, "format": "png", "data_base64": "aGVsbG8="}
            ]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(
            output.markdown,
            "\\![字面](image_0.png)\n\\` ![图](m/image_1.png)"
        );
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "m/image_1.png");
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14：围栏与行内代码内的 image_N.ext 字样是字面文本，不改写；
    // 正文真实引用照常加前缀并登记落盘。
    #[test]
    fn fenced_reference_is_literal_but_real_reference_is_rewritten() {
        let envelope = response(&json!({
            "content": "正文 ![图](image_0.png)\n\n```text\nimage_0.png\n```\n",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert!(output.markdown.contains("![图](m/image_0.png)"));
        assert!(output.markdown.contains("```text\nimage_0.png\n```"));
        assert_eq!(output.media.len(), 1);
    }

    // F28 / T-14/T-17：嵌套列表容器中的围栏可缩进 4 个及更多空格；
    // 空行不能把代码误判为正文，代码中的图片字面量零改写、零落盘。
    #[test]
    fn review_f28_nested_list_fences_preserve_literals_without_media() {
        for (list, indent) in [
            ("- 一级\n  - 二级\n", "    "),
            ("- 一级\n  - 二级\n    - 三级\n", "      "),
            ("- 一级\n  - 二级\n    - 三级\n      - 四级\n", "        "),
        ] {
            for fence in ["```", "~~~"] {
                for newline in ["\n", "\r\n"] {
                    let content = format!(
                        "{list}\n{indent}{fence}text\n{indent}![示例](image_0.png)\n\n{indent}![仍是代码](image_0.png)\n{indent}{fence}\n"
                    ).replace('\n', newline);
                    let envelope = response(&json!({
                        "content": content,
                        "images": [{"image_index": 0, "format": "png", "data": [0, 255, 37]}]
                    }));
                    let output = parse_document_response(&envelope, "nested_media").unwrap();
                    assert_eq!(
                        output.markdown.as_bytes(), content.as_bytes(),
                        "indent={}, fence={fence}, newline={newline:?}", indent.len()
                    );
                    assert!(output.media.is_empty(), "代码字面量不能登记待落盘图片");
                    assert!(output.warnings.is_empty());
                }
            }
        }
    }

    // F28 / T-14/T-17：同一索引在围栏中保持字面量、在正文中正常改写；
    // 独立代码图片不落盘，关闭围栏后的列表内及列表外真实图片不能被吞掉。
    #[test]
    fn review_f28_nested_list_fences_rewrite_only_real_images() {
        for (list, indent) in [
            ("- 一级\n  - 二级\n", "    "),
            ("- 一级\n  - 二级\n    - 三级\n", "      "),
        ] {
            for newline in ["\n", "\r\n"] {
                let content = format!(
                    "![前图](image_0.png)\n\n{list}\n{indent}```text\n{indent}![示例](image_0.png)\n\n{indent}![仅代码](image_1.png)\n{indent}```\n\n{indent}![列表图](image_2.png)\n\n![后图](image_0.png)\n"
                ).replace('\n', newline);
                let expected = format!(
                    "![前图](nested_media/image_0.png)\n\n{list}\n{indent}```text\n{indent}![示例](image_0.png)\n\n{indent}![仅代码](image_1.png)\n{indent}```\n\n{indent}![列表图](nested_media/image_2.png)\n\n![后图](nested_media/image_0.png)\n"
                ).replace('\n', newline);
                let envelope = response(&json!({
                    "content": content,
                    "images": [
                        {"image_index": 0, "format": "png", "data": [0, 255]},
                        {"image_index": 1, "format": "png", "data": [11, 22]},
                        {"image_index": 2, "format": "png", "data": [37, 42]}
                    ]
                }));
                let output = parse_document_response(&envelope, "nested_media").unwrap();
                assert_eq!(
                    output.markdown.as_bytes(), expected.as_bytes(),
                    "indent={}, newline={newline:?}", indent.len()
                );
                assert_eq!(output.media, vec![
                    MediaFile { relative: "nested_media/image_0.png".to_string(), bytes: vec![0, 255] },
                    MediaFile { relative: "nested_media/image_2.png".to_string(), bytes: vec![37, 42] },
                ]);
                assert!(output.warnings.is_empty());
            }
        }
    }

    // F28：有序列表、引用与混合容器只影响围栏定位，关闭或退出容器后恢复图片。
    #[test]
    fn review_f28_container_fences_keep_original_offsets_and_media() {
        for source in [
            "12. 项\n\n    ~~~\n    ![代码](image_0.png)\n\n    ~~~\n    ![真图](image_1.png)",
            "- > ```\n  > ![代码](image_0.png)\n  >\n  > ```\n  > ![真图](image_1.png)",
            "> - 项\n>\n>   ~~~\n>   ![代码](image_0.png)\n>\n>   ~~~\n>   ![真图](image_1.png)",
            "- 项\n     ~~~\n  ![代码](image_0.png)\n   ~~~\n  ![真图](image_1.png)",
            "- 项\n  ~~~\n  ![代码](image_0.png)\n![真图](image_1.png)",
        ] {
            for newline in ["\n", "\r\n"] {
                let content = source.replace('\n', newline);
                let expected = content.replace("![真图](image_1.png)", "![真图](m/image_1.png)");
                let envelope = response(&json!({
                    "content": content,
                    "images": [
                        {"image_index": 0, "format": "png", "data": [0, 255]},
                        {"image_index": 1, "format": "png", "data": [37, 128]}
                    ]
                }));
                let output = parse_document_response(&envelope, "m").unwrap();
                assert_eq!(output.markdown.as_bytes(), expected.as_bytes(), "{source:?}");
                assert_eq!(output.media, vec![MediaFile {
                    relative: "m/image_1.png".to_string(),
                    bytes: vec![37, 128],
                }]);
                assert!(output.warnings.is_empty());
            }
        }
    }

    // F28：不能全局取消围栏的三空格限制，也不能把分隔线识别成列表容器。
    #[test]
    fn review_f28_fence_indent_is_relative_to_real_containers_only() {
        for source in [
            "    ~~~\n\n![真图](image_0.png)\n",
            "- 项\n\n      ~~~\n\n  ![真图](image_0.png)\n",
            "- - -\n    ~~~\n\n![真图](image_0.png)\n",
            "***\n    ~~~\n\n![真图](image_0.png)\n",
            "- 项\n  - 子项\n\n列表外\n    ~~~\n\n![真图](image_0.png)\n",
        ] {
            let envelope = response(&json!({
                "content": source,
                "images": [{"image_index": 0, "format": "png", "data": [0, 255, 37]}]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown.as_bytes(),
                source.replace("![真图](image_0.png)", "![真图](m/image_0.png)").as_bytes());
            assert_eq!(output.media, vec![MediaFile {
                relative: "m/image_0.png".to_string(),
                bytes: vec![0, 255, 37],
            }]);
            assert!(output.warnings.is_empty());
        }
    }

    // 覆盖 T-14：正文未引用的图片不落盘（引擎引用↔字节一一对应口径）。
    #[test]
    fn unreferenced_image_is_not_materialized() {
        let envelope = response(&json!({
            "content": "正文没有图片\n",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "正文没有图片\n");
        assert!(output.media.is_empty());
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-16：被引用但字节缺失/损坏时保留占位引用并告警，不冒充成功。
    #[test]
    fn referenced_image_without_data_warns() {
        let envelope = response(&json!({
            "content": "![图](image_0.png)",
            "images": [{"image_index": 0, "format": "png"}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "![图](image_0.png)");
        assert!(output.media.is_empty());
        assert_eq!(
            output.warnings,
            vec!["图片 image_0.png 缺少有效数据，已保留占位引用"]
        );
    }

    // 覆盖 T-16：data_base64 缺失时回退 data 数字数组取字节。
    #[test]
    fn data_array_fallback_when_base64_absent() {
        let envelope = response(&json!({
            "content": "![图](image_0.png)",
            "images": [{"image_index": 0, "format": "png", "data": [104, 105]}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].bytes, b"hi".to_vec());
    }

    // 覆盖 T-18/T-24：引擎警告（auto_mode 自动降级）如实转达到任务界面；
    // exif 噪声按既有口径过滤，不改变其余文案。
    #[test]
    fn warnings_are_relayed_and_exif_noise_filtered() {
        let mut envelope = response(&json!({"content": "正文"}));
        envelope["warnings"] = json!([
            {"source": "exif", "message": "No EXIF data found"},
            {"source": "auto_mode",
             "message": "large document (520 pages > auto_fast_pages=500): OCR disabled for speed; set auto_fast_pages=0 to keep full quality"}
        ]);
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(
            output.warnings,
            vec!["auto_mode：large document (520 pages > auto_fast_pages=500): OCR disabled for speed; set auto_fast_pages=0 to keep full quality".to_string()]
        );
    }

    // 覆盖 T-16/T-24：协议异常必须计失败，不得拼出空产物冒充成功——缺
    // document、document 非对象、content 非字符串都要明确报错。
    #[test]
    fn protocol_shape_errors_are_explicit() {
        let missing = json!({"id": 1, "ok": true, "warnings": []});
        let error = parse_document_response(&missing, "m").unwrap_err();
        assert!(error.contains("协议异常"), "{error}");
        assert!(error.contains("document"), "{error}");
        let not_object = response(&json!("content"));
        let error = parse_document_response(&not_object, "m").unwrap_err();
        assert!(error.contains("必须是对象"), "{error}");
        let bad_content = response(&json!({"content": 3}));
        let error = parse_document_response(&bad_content, "m").unwrap_err();
        assert!(error.contains("content"), "{error}");
    }

    // 覆盖 T-16：images 字段类型异常时告警并保留正文，不按协议失败。
    #[test]
    fn invalid_images_field_warns() {
        let envelope = response(&json!({"content": "正文", "images": 3}));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "正文");
        assert!(output.media.is_empty());
        assert_eq!(
            output.warnings,
            vec!["图片资源字段类型无效，已保留占位引用"]
        );
    }

    // 覆盖 T-14/T-17：未闭合反引号是正文，不吞掉后面的真实图片；
    // 行内代码不能跨越空段落，alt 中的代码图片字面量也必须原样保留。
    #[test]
    fn unmatched_backticks_and_code_in_alt_do_not_hide_real_images() {
        for (content, expected) in [
            ("中文 `未闭合 ![图](image_0.png)",
             "中文 `未闭合 ![图](m/image_0.png)"),
            ("`未闭合\n\n![图](image_0.png)\n`",
             "`未闭合\n\n![图](m/image_0.png)\n`"),
            ("![说明 `![代码](image_0.png)`](image_1.png)",
             "![说明 `![代码](image_0.png)`](m/image_1.png)"),
        ] {
            let envelope = response(&json!({
                "content": content,
                "images": [
                    {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="},
                    {"image_index": 1, "format": "png", "data_base64": "aGVsbG8="}
                ]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown, expected);
            assert_eq!(output.media.len(), 1);
            assert!(output.warnings.is_empty());
        }
    }

    // 覆盖 T-14：合法 Windows 名称中的 &entity; 不能被阅读器当作字符引用，
    // Unicode 名称及多处引用保留，实际文件路径仍使用未经 URL 编码的名称。
    #[test]
    fn media_directory_ampersands_are_encoded() {
        let envelope = response(&json!({
            "content": "![甲](image_0.png) ![乙](image_0.png)",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "中文&copy;_media").unwrap();
        assert_eq!(output.markdown,
            "![甲](中文%26copy;_media/image_0.png) ![乙](中文%26copy;_media/image_0.png)");
        assert_eq!(output.media[0].relative, "中文&copy;_media/image_0.png");
        assert_eq!(output.media.len(), 1);
    }

    // 覆盖 T-14/T-16：严格标准 base64 的尾部位与 padding 边界，损坏时
    // data 数组回退仍保留；越界和非整数数组元素不能变成截断图片。
    #[test]
    fn image_byte_encoding_boundaries_are_explicit() {
        for (encoded, expected) in [
            ("AA==", vec![0]), ("AAA=", vec![0, 0]), ("AAAA", vec![0, 0, 0]),
            (" aG Vs\nbG8=\r", b"hello".to_vec()),
        ] {
            assert_eq!(decode_base64(encoded), Some(expected));
        }
        for encoded in ["", "A", "AA", "AAA", "AB==", "AAB=", "AA=A",
                        "=AAA", "AA==AAAA", "AAAA====", "中文", "AA-_"] {
            assert_eq!(decode_base64(encoded), None, "{encoded}");
        }
        assert_eq!(image_bytes(&json!({"data_base64": "AB==", "data": [0, 255]})),
                   Some(vec![0, 255]));
        for data in [json!([256]), json!([-1]), json!([1.5]), json!(["1"])] {
            assert_eq!(image_bytes(&json!({"data": data})), None);
        }
    }

    // 覆盖 T-14/T-17：反引号 info string 含反引号时不是围栏，
    // 真正围栏及其 Unicode info string、CRLF 和多反引号代码仍零改写。
    #[test]
    fn fence_and_code_delimiter_boundaries_preserve_literals() {
        for (content, expected) in [
            ("```语言`标签\n![图](image_0.png)",
             "```语言`标签\n![图](m/image_0.png)"),
            ("```中文\r\n![代码](image_0.png)\r\n```\r\n![图](image_0.png)",
             "```中文\r\n![代码](image_0.png)\r\n```\r\n![图](m/image_0.png)"),
            ("``说明 ` ![代码](image_0.png)`` ![图](image_0.png)",
             "``说明 ` ![代码](image_0.png)`` ![图](m/image_0.png)"),
        ] {
            let envelope = response(&json!({
                "content": content,
                "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown, expected);
            assert_eq!(output.media.len(), 1);
        }
    }

    // 覆盖 T-14/T-17：外部图片的 title 中含括号和图片字面量时，
    // 只改写其后的真实引用，不能截断 title 后把字面量当成图片。
    #[test]
    fn image_titles_with_parentheses_preserve_literal_references() {
        for content in [
            "![外图](external.png \"说明 ) ![字面](image_0.png)\") ![图](image_0.png)",
            "![外图](external_(1).png '说明 ) ![字面](image_0.png)') ![图](image_0.png)",
            "![外图](external.png (说明 ![字面](image_0.png))) ![图](image_0.png)",
        ] {
            let envelope = response(&json!({
                "content": content,
                "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            let (literal, _) = content.rsplit_once(" ![图]").unwrap();
            assert_eq!(output.markdown, format!("{literal} ![图](m/image_0.png)"));
            assert_eq!(output.media.len(), 1);
            assert!(output.warnings.is_empty());
        }
    }

    // 覆盖 T-14/T-17：普通链接的 title 是字面文本；label 内嵌图片
    // 仍是真实图片，不能为保护 title 而漏掉其媒体落盘。
    #[test]
    fn ordinary_link_titles_remain_literal_and_label_images_are_materialized() {
        let envelope = response(&json!({
            "content": "[外链](external.md \"说明 ![字面](image_0.png)\")\n[![图](image_0.png)](external.md '![字面](image_0.png)')",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown,
            "[外链](external.md \"说明 ![字面](image_0.png)\")\n[![图](m/image_0.png)](external.md '![字面](image_0.png)')");
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "m/image_0.png");
        assert_eq!(output.media[0].bytes, b"hello");
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14：CommonMark 尖括号目标同样是正文图片引用；
    // 目录前缀只进入目标，尖括号、title 和其余正文保持原样。
    #[test]
    fn angle_bracket_image_destinations_are_materialized() {
        let envelope = response(&json!({
            "content": "![图](<image_0.png> \"说明 )\")",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "![图](<m/image_0.png> \"说明 )\")");
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].relative, "m/image_0.png");
        assert_eq!(output.media[0].bytes, b"hello");
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14/T-17：貌似普通链接的 label 内开启跨行代码时，代码优先；
    // 字面图片既不能改写，也不能登记待落盘字节。
    #[test]
    fn review_fix_link_label_backtick_preserves_multiline_code_without_media() {
        for content in [
            "[说明 `字面](external.md)\n![代码](image_0.png)`",
            "[说明 `字面](external.md)\r\n![代码](image_0.png)`",
        ] {
            let envelope = response(&json!({
                "content": content,
                "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
            }));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown.as_bytes(), content.as_bytes());
            assert_eq!(output.media, Vec::<MediaFile>::new());
            assert!(output.warnings.is_empty());
        }
    }

    // 覆盖 T-14/T-17：平衡括号不代表合法普通链接；非法目标后的真实图片
    // 必须按引擎最终正文位置改写，并登记与 image_index 匹配的字节。
    #[test]
    fn review_fix_invalid_outer_link_materializes_nested_image() {
        let envelope = response(&json!({
            "content": "[说明](external.md ![图](image_0.png))",
            "images": [{"image_index": 0, "format": "png", "data_base64": "aGVsbG8="}]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, "[说明](external.md ![图](m/image_0.png))");
        assert_eq!(output.media, vec![MediaFile {
            relative: "m/image_0.png".to_string(),
            bytes: b"hello".to_vec(),
        }]);
        assert!(output.warnings.is_empty());
    }

    // 覆盖 T-14/T-17：非法外层链接不能吞掉图片，但合法 title 中的图片仍是
    // 字面量；合法 label 图片独立落盘，避免同一索引掩盖漏图或多落盘。
    #[test]
    fn review_fix_invalid_link_keeps_valid_titles_literal_and_label_media_correct() {
        let content = concat!(
            "[说明](external.md ![图](image_0.png))\n",
            "[双引号](external.md \"说明 ![字面](image_1.png)\")\n",
            "[单引号](external.md '说明 ![字面](image_1.png)')\n",
            "[括号](external.md (说明 ![字面](image_1.png)))\n",
            "[![标签](image_2.png)](external.md \"![字面](image_1.png)\")"
        );
        let envelope = response(&json!({
            "content": content,
            "images": [
                {"image_index": 0, "format": "png", "data_base64": "aGVsbG8="},
                {"image_index": 1, "format": "png", "data": [222, 173]},
                {"image_index": 2, "format": "png", "data": [0, 255, 37]}
            ]
        }));
        let output = parse_document_response(&envelope, "m").unwrap();
        assert_eq!(output.markdown, concat!(
            "[说明](external.md ![图](m/image_0.png))\n",
            "[双引号](external.md \"说明 ![字面](image_1.png)\")\n",
            "[单引号](external.md '说明 ![字面](image_1.png)')\n",
            "[括号](external.md (说明 ![字面](image_1.png)))\n",
            "[![标签](m/image_2.png)](external.md \"![字面](image_1.png)\")"
        ));
        assert_eq!(output.media, vec![
            MediaFile { relative: "m/image_0.png".to_string(), bytes: b"hello".to_vec() },
            MediaFile { relative: "m/image_2.png".to_string(), bytes: vec![0, 255, 37] },
        ]);
        assert!(output.warnings.is_empty());
    }

    // P3-13 / T-14/T-16/T-17：多图批次仅改写真实且有字节的目标；
    // 元数据顺序与引用顺序不同、缺字节/坏字节不能影响其他图片或字面量。
    #[test]
    fn review_remediation_multiple_images_preserve_literals_placeholders_and_warnings() {
        let content = concat!(
            "# 多图\r\n",
            "![二](<image_2.png> \"说明 ) ![title](image_5.png)\")\r\n",
            "```中文\r\n![围栏](image_5.png)\r\n```\r\n",
            "`跨行\n![代码](image_5.png)`\n",
            "![说明 `![alt](image_5.png)`](image_0.png)\n",
            "[![一](image_1.png)](external.md '![title](image_5.png)')\n",
            "\\![转义](image_5.png)\n",
            "![缺失](image_3.png) ![损坏](image_4.png)\n",
            "![再二](image_2.png) ![再零](image_0.png)"
        );
        let envelope = response(&json!({
            "content": content,
            "images": [
                {"image_index": 3, "format": "png"},
                {"image_index": 2, "format": "png", "data": [0, 255]},
                {"image_index": 5, "format": "png", "data_base64": "not-base64"},
                {"image_index": 4, "format": "png", "data": [256]},
                {"image_index": 1, "format": "png", "data_base64": "aGVsbG8="},
                {"image_index": 0, "format": "png", "data": [37]}
            ]
        }));
        let output = parse_document_response(&envelope, "中文_(1)&").unwrap();
        assert_eq!(output.markdown, concat!(
            "# 多图\r\n",
            "![二](<中文_%281%29%26/image_2.png> \"说明 ) ![title](image_5.png)\")\r\n",
            "```中文\r\n![围栏](image_5.png)\r\n```\r\n",
            "`跨行\n![代码](image_5.png)`\n",
            "![说明 `![alt](image_5.png)`](中文_%281%29%26/image_0.png)\n",
            "[![一](中文_%281%29%26/image_1.png)](external.md '![title](image_5.png)')\n",
            "\\![转义](image_5.png)\n",
            "![缺失](image_3.png) ![损坏](image_4.png)\n",
            "![再二](中文_%281%29%26/image_2.png) ![再零](中文_%281%29%26/image_0.png)"
        ));
        assert_eq!(output.media, vec![
            MediaFile { relative: "中文_(1)&/image_2.png".to_string(), bytes: vec![0, 255] },
            MediaFile { relative: "中文_(1)&/image_1.png".to_string(), bytes: b"hello".to_vec() },
            MediaFile { relative: "中文_(1)&/image_0.png".to_string(), bytes: vec![37] },
        ]);
        assert_eq!(output.warnings, vec![
            "图片 image_3.png 缺少有效数据，已保留占位引用",
            "图片 image_4.png 缺少有效数据，已保留占位引用",
        ], "未引用的损坏 image_5 不解码、不落盘、也不额外告警");
    }

    // P3-13 / T-14/T-17：稀疏正文引用与大量未引用元数据一起消费解析器。
    // 只断言正文/媒体语义，不用固定秒数或实现扫描/复制次数冒充性能证明。
    #[test]
    fn review_remediation_many_images_keep_sparse_references_exact() {
        for image_count in [16_u64, 64, 256] {
            let mut content = "正文无需规范化，空格  与 Unicode 保持原样。\r\n".repeat(512);
            let mut expected = content.clone();
            let mut expected_media = Vec::new();
            let mut images = Vec::new();
            for index in 0..image_count {
                images.push(json!({
                    "image_index": index, "format": "png", "data": [index % 256, 255]
                }));
                if index % 7 == 0 {
                    content.push_str(&format!("![图{index}](image_{index}.png)\n"));
                    expected.push_str(&format!("![图{index}](m/image_{index}.png)\n"));
                    expected_media.push(MediaFile {
                        relative: format!("m/image_{index}.png"),
                        bytes: vec![u8::try_from(index % 256).unwrap(), 255],
                    });
                } else {
                    let literal = format!("`![代码{index}](image_{index}.png)`\n");
                    content.push_str(&literal);
                    expected.push_str(&literal);
                }
            }
            let envelope = response(&json!({"content": content, "images": images}));
            let output = parse_document_response(&envelope, "m").unwrap();
            assert_eq!(output.markdown, expected);
            assert_eq!(output.media, expected_media);
            assert!(output.warnings.is_empty());
        }
    }
