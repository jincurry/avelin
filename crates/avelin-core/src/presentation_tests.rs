use super::*;

fn overrides(locale: Locale, entries: &[(&str, &str)]) -> Presentation {
    Presentation {
        strings: BTreeMap::from([(
            locale,
            entries
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        )]),
        ..Presentation::default()
    }
}

fn png_bytes(width: u32, height: u32, animated: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        if animated {
            encoder.set_animated(1, 0).unwrap();
        }
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&vec![0; width as usize * height as usize * 4])
            .unwrap();
        writer.finish().unwrap();
    }
    bytes
}

fn png_with_metadata(profile_bytes: usize, text_bytes: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut info = png::Info::with_size(1, 1);
        info.color_type = png::ColorType::Rgba;
        info.bit_depth = png::BitDepth::Eight;
        if profile_bytes > 0 {
            info.icc_profile = Some(std::borrow::Cow::Owned(vec![0; profile_bytes]));
        }
        let mut encoder = png::Encoder::with_info(&mut bytes, info).unwrap();
        if text_bytes > 0 {
            encoder
                .add_ztxt_chunk("Comment".into(), "a".repeat(text_bytes))
                .unwrap();
        }
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0; 4]).unwrap();
        writer.finish().unwrap();
    }
    bytes
}

#[test]
fn defaults_and_json_contract() {
    let presentation: Presentation = serde_json::from_str("{}").unwrap();
    assert_eq!(presentation, Presentation::default());
    presentation.validate().unwrap();
    let encoded = serde_json::to_value(&presentation).unwrap();
    assert_eq!(encoded["template"], "compact");
    assert_eq!(encoded["locale"], "auto");
    assert_eq!(encoded["fallback_locale"], "en-US");
    assert_eq!(encoded["theme"], "light");
    assert_eq!(encoded["accent_color"], "#36594C");
    for invalid in [
        r#"{"typo":true}"#,
        r#"{"locale":"fr-FR"}"#,
        r#"{"fallback_locale":"auto"}"#,
        r#"{"theme":"system"}"#,
        r#"{"strings":{"zh":{}}}"#,
    ] {
        assert!(
            serde_json::from_str::<Presentation>(invalid).is_err(),
            "{invalid}"
        );
    }
    let explicit: Presentation =
        serde_json::from_str(r#"{"locale":"zh-CN","theme":"dark","fallback_locale":"zh-CN"}"#)
            .unwrap();
    assert_eq!(explicit.locale, LocalePreference::Chinese);
    assert_eq!(explicit.theme, ThemeMode::Dark);
    assert_eq!(explicit.fallback_locale, Locale::Chinese);
    assert_eq!(
        serde_json::from_str::<Presentation>(&serde_json::to_string(&explicit).unwrap()).unwrap(),
        explicit
    );
}

#[test]
fn catalogs_have_the_same_complete_keys_and_placeholders() {
    assert_eq!(
        CHINESE.keys().collect::<Vec<_>>(),
        ENGLISH.keys().collect::<Vec<_>>()
    );
    // These are integration contracts used by the UI, not inferred per locale.
    for (key, parameter) in [
        ("window.title", "app_name"),
        ("brand.version", "version"),
        ("progress.percent", "percent"),
    ] {
        assert_eq!(
            placeholders(&ENGLISH[key]).unwrap(),
            BTreeSet::from([parameter])
        );
    }
    for (locale, catalog) in [(Locale::Chinese, &*CHINESE), (Locale::English, &*ENGLISH)] {
        Presentation {
            strings: BTreeMap::from([(locale, catalog.clone())]),
            ..Presentation::default()
        }
        .validate()
        .unwrap();
        for (key, text) in catalog {
            assert_eq!(
                placeholders(text).unwrap(),
                placeholders(&ENGLISH[key]).unwrap(),
                "{key}"
            );
        }
    }
}

#[test]
fn automatic_locale_and_explicit_preferences() {
    let mut presentation = Presentation::default();
    for tag in ["zh-CN", "ZH-cn", "zh_Hans_CN", "zh-TW", "zh"] {
        assert_eq!(
            presentation.resolve_locale(Some(tag)),
            Locale::Chinese,
            "{tag}"
        );
    }
    for tag in ["en-US", "EN-gb", "en_US.UTF-8", "en@custom", " en "] {
        assert_eq!(
            presentation.resolve_locale(Some(tag)),
            Locale::English,
            "{tag}"
        );
    }
    presentation.fallback_locale = Locale::Chinese;
    for tag in [
        None,
        Some(""),
        Some("fr-FR"),
        Some("english"),
        Some("zhuang"),
    ] {
        assert_eq!(presentation.resolve_locale(tag), Locale::Chinese);
    }
    presentation.locale = LocalePreference::English;
    assert_eq!(presentation.resolve_locale(Some("zh-CN")), Locale::English);
    presentation.locale = LocalePreference::Chinese;
    assert_eq!(presentation.resolve_locale(Some("en-US")), Locale::Chinese);
}

#[test]
fn partial_overrides_do_not_mix_languages() {
    let mut presentation = overrides(Locale::English, &[("action.install", "Install custom app")]);
    presentation.strings.insert(
        Locale::Chinese,
        BTreeMap::from([("brand.version".into(), "产品版本 {version}".into())]),
    );
    presentation.validate().unwrap();
    assert_eq!(
        presentation
            .text(Locale::Chinese, "action.install", &[])
            .unwrap(),
        "开始安装"
    );
    assert_eq!(
        presentation
            .text(Locale::English, "action.install", &[])
            .unwrap(),
        "Install custom app"
    );
    assert_eq!(
        presentation
            .text(Locale::Chinese, "brand.version", &[("version", "1.2")])
            .unwrap(),
        "产品版本 1.2"
    );
    assert_eq!(
        presentation
            .text(Locale::English, "brand.version", &[("version", "1.2")])
            .unwrap(),
        "Version 1.2"
    );
    assert_eq!(
        presentation
            .text(Locale::Chinese, "menu.copy", &[])
            .unwrap(),
        "复制"
    );
    assert_eq!(
        presentation
            .text(Locale::English, "notice.directory_required", &[])
            .unwrap(),
        "Enter an installation directory."
    );
    // Auto fallback chooses one full language, then applies that language's overlay.
    presentation.fallback_locale = Locale::Chinese;
    let locale = presentation.resolve_locale(Some("ja-JP"));
    assert_eq!(
        presentation.text(locale, "action.install", &[]).unwrap(),
        "开始安装"
    );
}

#[test]
fn overrides_reject_unknown_empty_or_invalid_placeholders() {
    for (key, value) in [
        ("action.unknown", "Hello"),
        ("action.install", "  \n"),
        ("action.install", "Install {app_name}"),
        ("brand.version", "Version"),
        ("brand.version", "{Version}"),
        ("brand.version", "{version:.2}"),
        ("brand.version", "{{version}}"),
        ("brand.version", "{version"),
        ("brand.version", "version}"),
        ("brand.version", "{}"),
        ("brand.version", "{version} {other}"),
    ] {
        assert!(
            overrides(Locale::English, &[(key, value)])
                .validate()
                .is_err(),
            "{key}: {value}"
        );
    }
    overrides(
        Locale::Chinese,
        &[("brand.version", "{version} / 版本 {version}")],
    )
    .validate()
    .unwrap();
    assert!(
        Presentation {
            template: "wizard".into(),
            ..Presentation::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn interpolation_checks_callsite_and_does_not_reparse_values() {
    let presentation = Presentation::default();
    assert_eq!(
        presentation
            .text(
                Locale::English,
                "window.title",
                &[("app_name", "{version} 中文")]
            )
            .unwrap(),
        "{version} 中文 · Setup"
    );
    assert_eq!(
        presentation
            .text(Locale::Chinese, "progress.percent", &[("percent", "42")])
            .unwrap(),
        "42%"
    );
    assert!(
        presentation
            .text(Locale::English, "missing.key", &[])
            .is_err()
    );
    assert!(
        presentation
            .text(Locale::English, "brand.version", &[])
            .is_err()
    );
    assert!(
        presentation
            .text(Locale::English, "action.install", &[("version", "1")])
            .is_err()
    );
    assert!(
        presentation
            .text(
                Locale::English,
                "brand.version",
                &[("version", "1"), ("version", "2")]
            )
            .is_err()
    );
    assert!(
        overrides(Locale::English, &[("brand.version", "{wrong}")])
            .text(Locale::English, "brand.version", &[("version", "1")])
            .is_err()
    );
}

#[test]
fn accent_is_strict_and_chosen_foreground_meets_contrast() {
    for color in [
        "red",
        "#abc",
        "#12345678",
        "#gggggg",
        " #36594C",
        "#36594C ",
        "＃36594C",
        "",
    ] {
        assert!(
            Presentation {
                accent_color: color.into(),
                ..Presentation::default()
            }
            .validate()
            .is_err(),
            "{color}"
        );
    }
    for red in (0..=255).step_by(17) {
        for green in (0..=255).step_by(17) {
            for blue in (0..=255).step_by(17) {
                let presentation = Presentation {
                    accent_color: format!("#{red:02x}{green:02x}{blue:02x}"),
                    ..Presentation::default()
                };
                let bg = relative_luminance(presentation.accent_rgb().unwrap());
                let fg = relative_luminance(presentation.accent_foreground_rgb().unwrap());
                assert!((bg.max(fg) + 0.05) / (bg.min(fg) + 0.05) >= 4.5);
            }
        }
    }
    assert_eq!(
        Presentation::default().accent_foreground_rgb().unwrap(),
        0xffffff
    );
}

#[test]
fn png_validation_decodes_pixels_and_enforces_bounds() {
    let valid = png_bytes(1, 1, false);
    let with_logo = |bytes| Presentation {
        logo_png: Some(bytes),
        ..Presentation::default()
    };
    with_logo(valid.clone()).validate().unwrap();
    with_logo(png_bytes(1024, 1024, false)).validate().unwrap();
    assert!(with_logo(png_bytes(1025, 1, false)).validate().is_err());
    assert!(with_logo(png_bytes(1, 1, true)).validate().is_err());
    assert!(with_logo(Vec::new()).validate().is_err());
    assert!(with_logo(vec![0; MAX_LOGO_BYTES + 1]).validate().is_err());
    assert!(with_logo(b"not a PNG".to_vec()).validate().is_err());
    let mut corrupt = valid.clone();
    let idat = corrupt
        .windows(4)
        .position(|window| window == b"IDAT")
        .unwrap();
    corrupt[idat + 4] ^= 0xff;
    assert!(with_logo(corrupt).validate().is_err());
    assert!(
        with_logo(valid[..valid.len() - 12].to_vec())
            .validate()
            .is_err()
    );
    let mut oversized = valid;
    oversized[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(with_logo(oversized).validate().is_err());
}

#[test]
fn documented_example_is_valid() {
    let example: Presentation = serde_json::from_str(include_str!(
        "../../../docs/schemas/presentation.example.json"
    ))
    .unwrap();
    example.validate().unwrap();
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../docs/schemas/presentation.schema.json"
    ))
    .unwrap();
    assert_eq!(
        schema["$defs"]["catalog"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<BTreeSet<_>>(),
        ENGLISH.keys().collect::<BTreeSet<_>>()
    );
}

#[test]
fn png_metadata_cannot_expand_unbounded_in_the_ui_decoder() {
    validate_logo(&png_with_metadata(64, 64)).unwrap();
    for bytes in [
        png_with_metadata(MAX_LOGO_BYTES + 1, 0),
        png_with_metadata(17 * 1024 * 1024, 0),
        png_with_metadata(0, MAX_LOGO_BYTES + 1),
        png_with_metadata(256 * 1024, 256 * 1024 + 1),
    ] {
        assert!(
            bytes.len() <= MAX_LOGO_BYTES,
            "fixture must pass the compressed file cap"
        );
        assert!(validate_logo(&bytes).is_err());
    }
    let mut trailing_data = png_bytes(1, 1, false);
    trailing_data.push(0);
    assert!(validate_logo(&trailing_data).is_err());
}
