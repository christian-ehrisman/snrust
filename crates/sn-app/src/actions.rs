//! Application-wide GPUI keyboard actions and macOS menu commands.

gpui_kit::actions!(
    snrust,
    [
        QuitApp,
        OpenCommandPalette,
        OpenInstanceManager,
        OpenPreferences,
        SaveActiveForm,
        UpdateAndCloseActiveForm,
        RefreshActiveView,
        ToggleCustomizeGrid,
        ToggleSidebar,
        ToggleSplitPane,
        ToggleThemeMode,
        IncreaseUiScale,
        DecreaseUiScale,
        ResetUiScale,
        CloseActiveTab,
        NewRecordInActiveTable,
    ]
);
