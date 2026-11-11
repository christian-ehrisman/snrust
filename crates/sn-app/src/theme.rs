//! Bespoke macOS-native Linear/Zed/Raycast-inspired palette & semantic ServiceNow badge colors.

use gpui_kit::component::theme::{ActiveTheme as _, Theme, ThemeMode};
use gpui_kit::{App, Hsla, hsla, px, rgb};

/// Applies `snrust`'s high-contrast, crisp desktop aesthetic over `gpui-component`'s theme tokens.
pub fn apply_snrust_theme(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    let theme = Theme::global_mut(cx);
    theme.font_size = px(13.0);
    theme.radius = px(6.0);
    theme.radius_lg = px(10.0);

    match mode {
        ThemeMode::Dark => {
            // Deep obsidian/slate inspired by Linear & Zed dark themes
            theme.colors.background = rgb(0x0D0F14).into();
            theme.colors.foreground = rgb(0xECEDF2).into();
            theme.colors.muted = rgb(0x161922).into();
            theme.colors.muted_foreground = rgb(0x8A91A6).into();
            theme.colors.popover = rgb(0x141720).into();
            theme.colors.popover_foreground = rgb(0xECEDF2).into();
            theme.colors.border = hsla(224.0 / 360.0, 0.18, 0.22, 0.65);
            theme.colors.input = hsla(224.0 / 360.0, 0.20, 0.24, 0.85);
            theme.colors.primary = rgb(0x5E6AD2).into();
            theme.colors.primary_hover = rgb(0x6E7AE2).into();
            theme.colors.primary_active = rgb(0x4E5AC0).into();
            theme.colors.primary_foreground = rgb(0xFFFFFF).into();
            theme.colors.secondary = rgb(0x1A1E29).into();
            theme.colors.secondary_hover = rgb(0x232837).into();
            theme.colors.secondary_active = rgb(0x2B3142).into();
            theme.colors.secondary_foreground = rgb(0xD8DCE8).into();
            theme.colors.accent = hsla(234.0 / 360.0, 0.56, 0.60, 0.18);
            theme.colors.accent_foreground = rgb(0xF4F5F8).into();
            theme.colors.sidebar = rgb(0x0A0C10).into();
            theme.colors.sidebar_foreground = rgb(0xD5D9E5).into();
            theme.colors.sidebar_border = hsla(224.0 / 360.0, 0.18, 0.20, 0.55);
            theme.colors.title_bar = rgb(0x0A0C10).into();
            theme.colors.title_bar_border = hsla(224.0 / 360.0, 0.18, 0.20, 0.55);
            theme.colors.tab_bar = rgb(0x0B0D12).into();
            theme.colors.table = rgb(0x0D0F14).into();
            theme.colors.table_head = rgb(0x12151D).into();
            theme.colors.table_head_foreground = rgb(0x949BB0).into();
            theme.colors.table_even = rgb(0x0F1218).into();
            theme.colors.table_hover = hsla(234.0 / 360.0, 0.45, 0.55, 0.10);
            theme.colors.table_active = hsla(234.0 / 360.0, 0.55, 0.60, 0.18);
        }
        ThemeMode::Light => {
            // Crisp alabaster/daylight inspired by Linear & macOS vibrancy
            theme.colors.background = rgb(0xFAFBFD).into();
            theme.colors.foreground = rgb(0x14171F).into();
            theme.colors.muted = rgb(0xF1F3F8).into();
            theme.colors.muted_foreground = rgb(0x61687A).into();
            theme.colors.popover = rgb(0xFFFFFF).into();
            theme.colors.popover_foreground = rgb(0x14171F).into();
            theme.colors.border = hsla(222.0 / 360.0, 0.16, 0.84, 0.90);
            theme.colors.input = hsla(222.0 / 360.0, 0.16, 0.80, 1.0);
            theme.colors.primary = rgb(0x4F5BD5).into();
            theme.colors.primary_hover = rgb(0x434EC4).into();
            theme.colors.primary_active = rgb(0x3842B0).into();
            theme.colors.primary_foreground = rgb(0xFFFFFF).into();
            theme.colors.secondary = rgb(0xEEF0F6).into();
            theme.colors.secondary_hover = rgb(0xE4E7F0).into();
            theme.colors.secondary_active = rgb(0xD9DDE9).into();
            theme.colors.secondary_foreground = rgb(0x1E2230).into();
            theme.colors.accent = hsla(234.0 / 360.0, 0.65, 0.56, 0.12);
            theme.colors.accent_foreground = rgb(0x14171F).into();
            theme.colors.sidebar = rgb(0xF3F5F9).into();
            theme.colors.sidebar_foreground = rgb(0x232735).into();
            theme.colors.sidebar_border = hsla(222.0 / 360.0, 0.16, 0.84, 0.90);
            theme.colors.title_bar = rgb(0xF3F5F9).into();
            theme.colors.title_bar_border = hsla(222.0 / 360.0, 0.16, 0.84, 0.90);
            theme.colors.tab_bar = rgb(0xF3F5F9).into();
            theme.colors.table = rgb(0xFFFFFF).into();
            theme.colors.table_head = rgb(0xF5F7FB).into();
            theme.colors.table_head_foreground = rgb(0x5A6175).into();
            theme.colors.table_even = rgb(0xFAFBFD).into();
            theme.colors.table_hover = hsla(234.0 / 360.0, 0.60, 0.56, 0.07);
            theme.colors.table_active = hsla(234.0 / 360.0, 0.65, 0.56, 0.14);
        }
    }
}

/// Semantic pill colors `(background, border, foreground)` for ServiceNow Priority values (`1`..`5`).
pub fn priority_badge_colors(raw_or_display: &str, cx: &App) -> (Hsla, Hsla, Hsla) {
    let dark = cx.theme().mode.is_dark();
    let norm = raw_or_display.trim().to_ascii_lowercase();

    if norm.starts_with('1') || norm.contains("critical") {
        if dark {
            (
                hsla(352. / 360., 0.82, 0.56, 0.18),
                hsla(352. / 360., 0.82, 0.60, 0.42),
                rgb(0xFDA4AF).into(),
            )
        } else {
            (
                hsla(352. / 360., 0.85, 0.50, 0.12),
                hsla(352. / 360., 0.80, 0.48, 0.35),
                rgb(0xBE123C).into(),
            )
        }
    } else if norm.starts_with('2') || norm.contains("high") {
        if dark {
            (
                hsla(28. / 360., 0.90, 0.55, 0.18),
                hsla(28. / 360., 0.90, 0.58, 0.42),
                rgb(0xFDBA74).into(),
            )
        } else {
            (
                hsla(28. / 360., 0.90, 0.50, 0.13),
                hsla(28. / 360., 0.85, 0.45, 0.35),
                rgb(0xC2410C).into(),
            )
        }
    } else if norm.starts_with('3') || norm.contains("moderate") {
        if dark {
            (
                hsla(45. / 360., 0.90, 0.52, 0.16),
                hsla(45. / 360., 0.85, 0.55, 0.38),
                rgb(0xFDE047).into(),
            )
        } else {
            (
                hsla(45. / 360., 0.92, 0.46, 0.15),
                hsla(45. / 360., 0.85, 0.40, 0.35),
                rgb(0xA16207).into(),
            )
        }
    } else if norm.starts_with('4') || norm.contains("low") {
        if dark {
            (
                hsla(205. / 360., 0.82, 0.55, 0.16),
                hsla(205. / 360., 0.78, 0.58, 0.35),
                rgb(0x7DD3FC).into(),
            )
        } else {
            (
                hsla(205. / 360., 0.85, 0.48, 0.12),
                hsla(205. / 360., 0.80, 0.45, 0.32),
                rgb(0x0369A1).into(),
            )
        }
    } else {
        (
            cx.theme().secondary,
            cx.theme().border,
            cx.theme().muted_foreground,
        )
    }
}

/// Semantic pill colors `(background, border, foreground)` for ServiceNow `state` / lifecycle fields.
pub fn state_badge_colors(display_val: &str, cx: &App) -> (Hsla, Hsla, Hsla) {
    let dark = cx.theme().mode.is_dark();
    let norm = display_val.trim().to_ascii_lowercase();

    if norm.contains("progress") || norm.contains("active") || norm.contains("implement") {
        if dark {
            (
                hsla(217. / 360., 0.88, 0.60, 0.18),
                hsla(217. / 360., 0.85, 0.62, 0.42),
                rgb(0x93C5FD).into(),
            )
        } else {
            (
                hsla(217. / 360., 0.88, 0.52, 0.12),
                hsla(217. / 360., 0.80, 0.48, 0.32),
                rgb(0x1D4ED8).into(),
            )
        }
    } else if norm.contains("resolved") || norm.contains("complete") {
        if dark {
            (
                hsla(152. / 360., 0.72, 0.48, 0.18),
                hsla(152. / 360., 0.72, 0.52, 0.40),
                rgb(0x6EE7B7).into(),
            )
        } else {
            (
                hsla(152. / 360., 0.76, 0.40, 0.12),
                hsla(152. / 360., 0.72, 0.36, 0.32),
                rgb(0x047857).into(),
            )
        }
    } else if norm.contains("hold") || norm.contains("pending") || norm.contains("awaiting") {
        if dark {
            (
                hsla(38. / 360., 0.90, 0.54, 0.17),
                hsla(38. / 360., 0.88, 0.56, 0.40),
                rgb(0xFCD34D).into(),
            )
        } else {
            (
                hsla(38. / 360., 0.90, 0.48, 0.14),
                hsla(38. / 360., 0.85, 0.42, 0.35),
                rgb(0xB45309).into(),
            )
        }
    } else if norm == "new" || norm.contains("open") {
        if dark {
            (
                hsla(265. / 360., 0.78, 0.64, 0.18),
                hsla(265. / 360., 0.75, 0.66, 0.40),
                rgb(0xC4B5FD).into(),
            )
        } else {
            (
                hsla(265. / 360., 0.78, 0.55, 0.12),
                hsla(265. / 360., 0.72, 0.50, 0.32),
                rgb(0x6D28D9).into(),
            )
        }
    } else {
        (
            cx.theme().secondary,
            cx.theme().border,
            cx.theme().muted_foreground,
        )
    }
}

/// Parses a `#RRGGBB` hex string into an `Hsla` color with safe fallback.
pub fn parse_hex_color(hex: &str) -> Option<Hsla> {
    let s = hex.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let val = u32::from_str_radix(s, 16).ok()?;
    Some(rgb(val).into())
}
