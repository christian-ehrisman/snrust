//! Application-wide GPUI keyboard actions and macOS menu commands.

gpui_kit::actions!(
    snrust,
    [
        QuitApp,
        OpenCommandPalette,
        OpenInstanceManager,
        SaveActiveForm,
        UpdateAndCloseActiveForm,
        RefreshActiveView,
        ToggleCustomizeGrid,
        ToggleSidebar,
        ToggleSplitPane,
        ToggleThemeMode,
        CloseActiveTab,
        NewRecordInActiveTable,
    ]
);
