//! Data-only presentation settings shared by packaging and the native UI.
//!
//! Built-in catalogs are complete. A partial override only changes its own
//! language; an English override must not leak into a Chinese screen. The
//! fallback locale selects the language when automatic detection cannot match.
//! Execution diagnostics are deliberately outside these UI catalogs.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    sync::LazyLock,
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const MAX_LOGO_BYTES: usize = 512 * 1024;
pub const MAX_LOGO_DIMENSION: u32 = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Locale {
    #[serde(rename = "zh-CN")]
    Chinese,
    #[default]
    #[serde(rename = "en-US")]
    English,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalePreference {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "zh-CN")]
    Chinese,
    #[serde(rename = "en-US")]
    English,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Presentation {
    pub template: String,
    pub locale: LocalePreference,
    pub fallback_locale: Locale,
    pub theme: ThemeMode,
    pub accent_color: String,
    /// Embedded PNG bytes, not a path or URL. Only static PNGs are accepted.
    pub logo_png: Option<Vec<u8>>,
    /// Partial UI translations, keyed by the registered names in the catalogs.
    pub strings: BTreeMap<Locale, BTreeMap<String, String>>,
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            template: "compact".into(),
            locale: LocalePreference::Auto,
            fallback_locale: Locale::English,
            theme: ThemeMode::Light,
            accent_color: "#36594C".into(),
            logo_png: None,
            strings: BTreeMap::new(),
        }
    }
}

type Catalog = BTreeMap<String, String>;
static CHINESE: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("locales/zh-CN.json"))
        .expect("built-in Chinese catalog is valid JSON")
});
static ENGLISH: LazyLock<Catalog> = LazyLock::new(|| {
    serde_json::from_str(include_str!("locales/en-US.json"))
        .expect("built-in English catalog is valid JSON")
});

/// The complete registered UI catalog, also useful to translation editors.
pub fn builtin_strings(locale: Locale) -> &'static BTreeMap<String, String> {
    match locale {
        Locale::Chinese => &CHINESE,
        Locale::English => &ENGLISH,
    }
}

impl Presentation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.template == "compact",
            "unsupported presentation template: {}",
            self.template
        );
        self.accent_rgb()?;
        if let Some(logo) = &self.logo_png {
            validate_logo(logo).context("invalid presentation logo_png")?;
        }
        for (locale, overrides) in &self.strings {
            for (key, value) in overrides {
                let original = ENGLISH
                    .get(key)
                    .with_context(|| format!("unknown presentation string key: {key}"))?;
                ensure!(
                    !value.trim().is_empty(),
                    "empty presentation string: {locale:?}/{key}"
                );
                let actual = placeholders(value)
                    .with_context(|| format!("invalid presentation string: {locale:?}/{key}"))?;
                ensure!(
                    actual == placeholders(original)?,
                    "placeholder mismatch in presentation string: {locale:?}/{key}"
                );
            }
        }
        Ok(())
    }

    /// Explicit preferences win. Auto accepts zh/en language tags, including
    /// Windows tags and common POSIX forms (e.g. en_US.UTF-8); otherwise fallback.
    pub fn resolve_locale(&self, system_locale: Option<&str>) -> Locale {
        match self.locale {
            LocalePreference::Chinese => Locale::Chinese,
            LocalePreference::English => Locale::English,
            LocalePreference::Auto => {
                let language = system_locale
                    .unwrap_or("")
                    .trim()
                    .split(['-', '_', '.', '@'])
                    .next()
                    .unwrap_or("");
                if language.eq_ignore_ascii_case("zh") {
                    Locale::Chinese
                } else if language.eq_ignore_ascii_case("en") {
                    Locale::English
                } else {
                    self.fallback_locale
                }
            }
        }
    }

    /// Resolve and interpolate a registered UI string. Unknown keys, missing,
    /// duplicate or extra arguments are errors, never plausible display text.
    /// Values are inserted once as plain text, not parsed as new placeholders.
    ///
    /// Precedence: requested override, requested built-in, fallback override,
    /// English built-in. The last two are a defensive path; both shipped
    /// catalogs are complete, so partial overrides never change the language.
    pub fn text(&self, locale: Locale, key: &str, args: &[(&str, &str)]) -> Result<String> {
        let original = ENGLISH
            .get(key)
            .with_context(|| format!("unknown presentation string key: {key}"))?;
        let expected = placeholders(original)?;
        let mut values = BTreeMap::new();
        for &(name, value) in args {
            ensure!(
                values.insert(name, value).is_none(),
                "duplicate text argument: {name}"
            );
        }
        ensure!(
            expected == values.keys().copied().collect::<BTreeSet<_>>(),
            "text argument mismatch for: {key}"
        );
        let template = self
            .strings
            .get(&locale)
            .and_then(|map| map.get(key))
            .or_else(|| builtin_strings(locale).get(key))
            .or_else(|| {
                self.strings
                    .get(&self.fallback_locale)
                    .and_then(|map| map.get(key))
            })
            .unwrap_or(original);
        ensure!(
            !template.trim().is_empty(),
            "empty presentation string: {key}"
        );
        ensure!(
            placeholders(template)? == expected,
            "placeholder mismatch for: {key}"
        );
        let mut result = String::with_capacity(template.len());
        let mut remaining = template.as_str();
        while let Some(start) = remaining.find('{') {
            result.push_str(&remaining[..start]);
            let end = remaining[start..]
                .find('}')
                .expect("placeholders already validated")
                + start;
            result.push_str(values[&remaining[start + 1..end]]);
            remaining = &remaining[end + 1..];
        }
        result.push_str(remaining);
        Ok(result)
    }

    /// Parse the opaque accent color. Alpha, shorthand, names and whitespace
    /// are deliberately excluded so every consumer renders the same RGB value.
    pub fn accent_rgb(&self) -> Result<u32> {
        let bytes = self.accent_color.as_bytes();
        ensure!(
            bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit),
            "accent_color must be exactly #RRGGBB"
        );
        Ok(u32::from_str_radix(&self.accent_color[1..], 16)?)
    }

    /// Choose black or white text with the greater WCAG contrast against the
    /// opaque accent. One of these always exceeds 4.5:1 for an sRGB color.
    pub fn accent_foreground_rgb(&self) -> Result<u32> {
        let luminance = relative_luminance(self.accent_rgb()?);
        let black = (luminance + 0.05) / 0.05;
        let white = 1.05 / (luminance + 0.05);
        Ok(if black >= white { 0x000000 } else { 0xffffff })
    }
}

/// Braces have one meaning: {lower_snake_case_name}. Literal, unmatched,
/// nested, format-specifier and empty braces are rejected, not guessed at.
fn placeholders(text: &str) -> Result<BTreeSet<&str>> {
    let mut names = BTreeSet::new();
    let mut remaining = text;
    while let Some(start) = remaining.find(['{', '}']) {
        ensure!(
            remaining.as_bytes()[start] == b'{',
            "unmatched closing brace"
        );
        let end = remaining[start..]
            .find('}')
            .context("unclosed placeholder")?
            + start;
        let name = &remaining[start + 1..end];
        ensure!(
            !name.is_empty()
                && name.as_bytes()[0].is_ascii_lowercase()
                && name
                    .bytes()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_'),
            "invalid placeholder name: {name}"
        );
        names.insert(name);
        remaining = &remaining[end + 1..];
    }
    Ok(names)
}

fn relative_luminance(rgb: u32) -> f64 {
    let linear = |channel: u32| {
        let value = channel as f64 / 255.;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear((rgb >> 16) & 255)
        + 0.7152 * linear((rgb >> 8) & 255)
        + 0.0722 * linear(rgb & 255)
}

fn validate_logo(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_LOGO_BYTES,
        "logo must be 1..=512 KiB"
    );
    // Check the fixed IHDR prefix before any decoder allocation or decompression.
    ensure!(
        bytes.len() >= 33
            && &bytes[..8] == b"\x89PNG\r\n\x1a\n"
            && bytes[8..12] == [0, 0, 0, 13]
            && &bytes[12..16] == b"IHDR",
        "invalid PNG header"
    );
    let width = u32::from_be_bytes(bytes[16..20].try_into()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into()?);
    ensure!(
        (1..=MAX_LOGO_DIMENSION).contains(&width) && (1..=MAX_LOGO_DIMENSION).contains(&height),
        "logo dimensions must be 1..=1024 pixels"
    );
    let mut offset = 8;
    let mut has_profile = false;
    loop {
        ensure!(bytes.len() - offset >= 12, "truncated PNG chunk");
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into()?) as usize;
        let end = offset
            .checked_add(length)
            .and_then(|end| end.checked_add(12))
            .context("PNG chunk length overflow")?;
        ensure!(end <= bytes.len(), "truncated PNG chunk payload");
        let kind = &bytes[offset + 4..offset + 8];
        has_profile |= kind == b"iCCP";
        if kind == b"IEND" {
            ensure!(
                length == 0 && end == bytes.len(),
                "invalid PNG end or trailing data"
            );
            break;
        }
        offset = end;
    }
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(false);
    options.set_skip_ancillary_crc_failures(false);
    // Validate metadata too: the UI's independent PNG decoder may read it.
    // Compressed text is inflated below under a separate cumulative limit.
    options.set_ignore_text_chunk(false);
    options.set_ignore_iccp_chunk(false);
    let mut decoder = png::Decoder::new_with_options(Cursor::new(bytes), options);
    decoder.set_limits(png::Limits {
        bytes: 16 * 1024 * 1024,
    });
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("read PNG information")?;
    // png deliberately ignores malformed/over-limit iCCP errors. Do not allow
    // such a file to reach a later decoder with a more permissive memory limit.
    ensure!(
        !has_profile || reader.info().icc_profile.is_some(),
        "PNG color profile is invalid or exceeds decoder limits"
    );
    ensure!(
        reader.info().animation_control.is_none(),
        "animated PNG logos are unsupported"
    );
    let size = reader
        .output_buffer_size()
        .context("PNG output size overflow")?;
    ensure!(
        size <= 4 * MAX_LOGO_DIMENSION as usize * MAX_LOGO_DIMENSION as usize,
        "PNG decoded pixel buffer exceeds limit"
    );
    let mut buffer = vec![0; size];
    reader
        .next_frame(&mut buffer)
        .context("decode PNG pixels")?;
    reader.finish().context("finish PNG validation")?;
    let info = reader.info();
    let mut metadata_remaining = MAX_LOGO_BYTES;
    if let Some(profile) = &info.icc_profile {
        metadata_remaining = metadata_remaining
            .checked_sub(profile.len())
            .context("PNG decoded metadata exceeds 512 KiB")?;
    }
    for text in &info.uncompressed_latin1_text {
        metadata_remaining = metadata_remaining
            .checked_sub(text.text.len())
            .context("PNG decoded metadata exceeds 512 KiB")?;
    }
    for text in &info.compressed_latin1_text {
        let mut text = text.clone();
        text.decompress_text_with_limit(metadata_remaining)
            .context("decode PNG text")?;
        metadata_remaining = metadata_remaining
            .checked_sub(text.get_text()?.len())
            .context("PNG decoded metadata exceeds 512 KiB")?;
    }
    for text in &info.utf8_text {
        let mut text = text.clone();
        text.decompress_text_with_limit(metadata_remaining)
            .context("decode PNG UTF-8 text")?;
        metadata_remaining = metadata_remaining
            .checked_sub(text.get_text()?.len())
            .context("PNG decoded metadata exceeds 512 KiB")?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
