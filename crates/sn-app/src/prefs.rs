//! Persisted user preferences, shared with every view as a GPUI global.

use gpui_kit::component::Size;
use gpui_kit::component::theme::ThemeMode;
use gpui_kit::{App, Context, Global, Subscription, Window, WindowAppearance};
use serde::{Deserialize, Serialize};
use sn_cache::SnCache;

const PREFS_KEY: &str = "preferences";

pub const MIN_UI_SCALE: f32 = 0.8;
pub const MAX_UI_SCALE: f32 = 1.5;
pub const UI_SCALE_STEP: f32 = 0.1;
pub const LIST_PAGE_SIZES: [usize; 4] = [25, 50, 100, 200];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    System,
    Light,
    #[default]
    Dark,
}

impl Appearance {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "Match system",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn from_key(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|appearance| appearance.key() == key)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    Compact,
    #[default]
    Comfortable,
}

impl Density {
    pub const ALL: [Self; 2] = [Self::Compact, Self::Comfortable];

    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Comfortable => "Comfortable",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Comfortable => "comfortable",
        }
    }

    pub fn from_key(key: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|density| density.key() == key)
            .unwrap_or_default()
    }
}

/// Every field is defaulted so preferences written by an older build keep loading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPrefs {
    pub appearance: Appearance,
    /// Multiplier applied to text and to sizes that hold text.
    pub ui_scale: f32,
    pub density: Density,
    pub list_page_size: usize,
    /// Cache and network timing readouts in list and form chrome.
    pub show_perf_telemetry: bool,
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self {
            appearance: Appearance::default(),
            ui_scale: 1.0,
            density: Density::default(),
            list_page_size: 100,
            show_perf_telemetry: true,
        }
    }
}

impl AppPrefs {
    /// Hand-edited or corrupted values must not produce an unusable window.
    fn sanitized(mut self) -> Self {
        self.ui_scale = if self.ui_scale.is_finite() {
            // One decimal place keeps repeated stepping from drifting.
            (self.ui_scale.clamp(MIN_UI_SCALE, MAX_UI_SCALE) * 10.0).round() / 10.0
        } else {
            1.0
        };
        if !LIST_PAGE_SIZES.contains(&self.list_page_size) {
            self.list_page_size = 100;
        }
        self
    }

    pub fn theme_mode(&self, system: WindowAppearance) -> ThemeMode {
        match self.appearance {
            Appearance::Light => ThemeMode::Light,
            Appearance::Dark => ThemeMode::Dark,
            Appearance::System => match system {
                WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeMode::Dark,
                WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeMode::Light,
            },
        }
    }

    /// Table row size for the chosen density. Row heights are fixed in pixels by the
    /// component library, so larger UI scales step up a size to keep text from clipping.
    pub fn table_size(&self) -> Size {
        let enlarged = if self.ui_scale > 1.3 {
            2
        } else if self.ui_scale > 1.1 {
            1
        } else {
            0
        };
        let step = enlarged
            + match self.density {
                Density::Compact => 0,
                Density::Comfortable => 1,
            };
        match step {
            0 => Size::XSmall,
            1 => Size::Small,
            2 => Size::Medium,
            _ => Size::Large,
        }
    }

    /// Runs `on_change` on the view whenever any preference changes.
    pub fn observe_in<V: 'static>(
        window: &Window,
        cx: &mut Context<V>,
        on_change: impl FnMut(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Subscription {
        cx.observe_global_in::<PrefsStore>(window, on_change)
    }

    /// Installs the global. Unreadable preferences fall back to defaults instead of
    /// blocking launch.
    pub fn install(cache: SnCache, cx: &mut App) {
        let prefs = match cache.get_setting::<AppPrefs>(PREFS_KEY) {
            Ok(saved) => saved.unwrap_or_default(),
            Err(err) => {
                tracing::warn!(%err, "Cannot read preferences; using defaults");
                AppPrefs::default()
            }
        }
        .sanitized();
        cx.set_global(PrefsStore { prefs, cache });
    }

    pub fn get(cx: &App) -> &AppPrefs {
        &cx.global::<PrefsStore>().prefs
    }

    /// Applies `change`, persists it, restyles, and repaints every window.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut AppPrefs)) {
        let store = cx.global_mut::<PrefsStore>();
        let mut next = store.prefs.clone();
        change(&mut next);
        let next = next.sanitized();
        if next == store.prefs {
            return;
        }
        store.prefs = next.clone();
        if let Err(err) = store.cache.set_setting(PREFS_KEY, &next) {
            tracing::warn!(%err, "Cannot save preferences");
        }
        crate::theme::apply_snrust_theme(cx);
        cx.refresh_windows();
    }

    pub fn step_ui_scale(cx: &mut App, steps: i32) {
        Self::update(cx, |prefs| {
            prefs.ui_scale += steps as f32 * UI_SCALE_STEP;
        });
    }
}

struct PrefsStore {
    prefs: AppPrefs,
    cache: SnCache,
}

impl Global for PrefsStore {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_values_are_repaired() {
        let prefs = AppPrefs {
            ui_scale: 9.0,
            list_page_size: 7,
            ..AppPrefs::default()
        }
        .sanitized();
        assert_eq!(prefs.ui_scale, MAX_UI_SCALE);
        assert_eq!(prefs.list_page_size, 100);

        let prefs = AppPrefs {
            ui_scale: f32::NAN,
            ..AppPrefs::default()
        }
        .sanitized();
        assert_eq!(prefs.ui_scale, 1.0);
    }

    #[test]
    fn stepping_scale_does_not_accumulate_float_error() {
        let mut prefs = AppPrefs::default();
        for _ in 0..3 {
            prefs.ui_scale += UI_SCALE_STEP;
            prefs = prefs.sanitized();
        }
        assert_eq!(prefs.ui_scale, 1.3);
    }

    #[test]
    fn preferences_from_an_older_build_fill_in_defaults() {
        let prefs: AppPrefs = serde_json::from_str(r#"{"appearance":"light"}"#).unwrap();
        assert_eq!(prefs.appearance, Appearance::Light);
        assert_eq!(prefs.ui_scale, 1.0);
        assert_eq!(prefs.list_page_size, 100);
    }

    #[test]
    fn table_rows_grow_with_density_and_scale() {
        let mut prefs = AppPrefs {
            density: Density::Compact,
            ..AppPrefs::default()
        };
        assert_eq!(prefs.table_size(), Size::XSmall);
        prefs.density = Density::Comfortable;
        assert_eq!(prefs.table_size(), Size::Small);
        prefs.ui_scale = 1.5;
        assert_eq!(prefs.table_size(), Size::Large);
    }

    #[test]
    fn system_appearance_follows_the_window() {
        let prefs = AppPrefs {
            appearance: Appearance::System,
            ..AppPrefs::default()
        };
        assert_eq!(
            prefs.theme_mode(WindowAppearance::VibrantLight),
            ThemeMode::Light
        );
        assert_eq!(prefs.theme_mode(WindowAppearance::Dark), ThemeMode::Dark);
        let forced = AppPrefs::default();
        assert_eq!(forced.theme_mode(WindowAppearance::Light), ThemeMode::Dark);
    }
}
