using System.Globalization;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.Graphics;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private Window? _settingsWindow;
    private bool _closingSettingsDialog;
    private bool _allowViewerClose;
    private bool _savingAll;
    private string? _settingsSaveResult;
    private readonly HashSet<TextBox> _trackedNumberEditors = new();

    private void EnsureSettingsWindow()
    {
        if (_settingsWindow is not null) return;
        // Keep the existing form/controller and draft instances. Only the visual
        // host changes, so changing tabs or hiding the window never reloads edits.
        WorkspaceGrid.Children.Remove(SettingsPane);
        Grid.SetColumn(SettingsPane, 0);
        _settingsWindow = new Window { Title = "AutoPierCam settings", Content = SettingsPane };
        _settingsWindow.AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "autopiercam.ico"));
        var work = DisplayArea.GetFromWindowId(AppWindow.Id, DisplayAreaFallback.Primary).WorkArea;
        double scale = Content.XamlRoot.RasterizationScale;
        var size = SettingsLayout.SettingsWindowSize(scale, work.Width, work.Height);
        _settingsWindow.AppWindow.Resize(new SizeInt32(size.Width, size.Height));
        _settingsWindow.AppWindow.Closing += async (_, e) => {
            if (_closed) return;
            e.Cancel = true;
            if (await CanLeaveSettingsAsync()) SetSettingsVisible(false);
        };
    }

    private async void ViewerClosing(AppWindow sender, AppWindowClosingEventArgs e)
    {
        if (_allowViewerClose) return;
        _hasUnsavedSettings = _settingsBaseline is not null && ReadSettingsForm() != _settingsBaseline;
        if (!_hasUnsavedSettings && !SharingHasEdits && !_savingAll && !_sharingBusy && !_operationInProgress) return;
        e.Cancel = true;
        SetSettingsVisible(true);
        if (await CanLeaveSettingsAsync()) { _allowViewerClose = true; Close(); }
    }

    private async Task<bool> CanLeaveSettingsAsync()
    {
        if (_closingSettingsDialog || _savingAll || _sharingBusy || _operationInProgress) return false;
        _hasUnsavedSettings = _settingsBaseline is not null && ReadSettingsForm() != _settingsBaseline;
        SharingChanged();
        if (!_hasUnsavedSettings && !SharingHasEdits) return true;
        _closingSettingsDialog = true;
        try
        {
            var dialog = new ContentDialog {
                XamlRoot = SettingsPane.XamlRoot, Title = "Unsaved settings",
                Content = "Imaging and Chatstronomy have a shared Save. Save both tabs before closing?",
                PrimaryButtonText = "Save all", SecondaryButtonText = "Discard all",
                CloseButtonText = "Keep editing", DefaultButton = ContentDialogButton.Close,
            };
            var result = await dialog.ShowAsync();
            if (result == ContentDialogResult.Primary) return await SaveAllSettingsAsync();
            if (result == ContentDialogResult.Secondary) return await DiscardAllSettingsAsync();
            return false;
        }
        finally { _closingSettingsDialog = false; }
    }

    private void UpdateSharedSave()
    {
        if (!_sharingInitialized) return;
        bool captureDirty = _hasUnsavedSettings;
        bool sharingDirty = SharingHasEdits || _sharing?.HasChatOverrides == true;
        bool idle = !_closed && !_operationInProgress && !_sharingBusy && !_savingAll;
        SaveButton.IsEnabled = idle && (captureDirty || sharingDirty)
            && (!captureDirty || (_configurationSnapshot is not null && !_configurationNeedsRefresh && !_captureNeedsReview && !_liveStatusUnavailable))
            && (!sharingDirty || (SharingSupported && !_sharingStatusUnknown && _sharing?.NeedsReview == false));
        CaptureDiscardButton.IsEnabled = idle && (captureDirty || SharingHasEdits || _captureNeedsReview || _sharing?.NeedsReview == true);
        SettingsSaveSummary.Text = _settingsSaveResult ?? (_savingAll ? "Saving both tabs…"
            : captureDirty && sharingDirty ? "Unsaved changes: Imaging and Chatstronomy."
            : captureDirty ? "Unsaved changes: Imaging."
            : sharingDirty ? "Unsaved changes: Chatstronomy."
            : "Both tabs are up to date. Save applies changes from both tabs.");
    }

    private async Task<bool> SaveAllSettingsAsync()
    {
        if (_savingAll || _operationInProgress || _sharingBusy || _closed) return false;
        // Read live text, including the editor which still has keyboard focus.
        _hasUnsavedSettings = _settingsBaseline is not null && ReadSettingsForm() != _settingsBaseline;
        SharingChanged();
        bool capture = _hasUnsavedSettings;
        bool sharing = SharingHasEdits || _sharing?.HasChatOverrides == true;
        _settingsSaveResult = null;
        if (ConfigInfoBar.Severity == InfoBarSeverity.Success) ConfigInfoBar.IsOpen = false;
        _savingAll = true;
        SetControlsForOperation(false);
        try
        {
            var result = await SettingsSaveBatch.RunAsync(capture, sharing,
                validateImaging: () => {
                    if (_configurationSnapshot is null || _configurationNeedsRefresh || _captureNeedsReview || _liveStatusUnavailable)
                        throw new InvalidOperationException("Reload or review Imaging settings before saving.");
                    _ = BuildConfigurationFromInputs(_configurationSnapshot.Config);
                },
                validateSharing: () => {
                    if (_sharing is null || _sharingStatusUnknown) throw new InvalidOperationException("Reload Chatstronomy before saving.");
                    _sharing.Draft = SharingInputs();
                    _ = _sharing.ForSave();
                },
                saveImaging: async () => {
                    bool confirmed = false;
                    await RunUiOperationAsync("Saving Imaging settings…", async ct => {
                        await SaveConfigurationAsync(ct);
                        confirmed = true;
                    });
                    return confirmed;
                },
                saveSharing: () => RunSharingOperationAsync("Saving Chatstronomy settings…", SaveSharingAsync));
            _settingsSaveResult = result.Message;
            if (result.Success)
            {
                // The common result replaces per-tab success banners.
                ConfigInfoBar.IsOpen = false;
                _sharingFeedback = null;
            }
            return result.Success;
        }
        finally { _savingAll = false; SetControlsForOperation(false); }
    }

    private async Task<bool> DiscardAllSettingsAsync()
    {
        if (_savingAll || _operationInProgress || _sharingBusy) return false;
        // A revision conflict needs a fresh baseline, not the stale snapshot
        // which caused it. Failed reloads leave drafts intact and keep us open.
        if (_captureNeedsReview || _configurationNeedsRefresh)
        {
            await RunUiOperationAsync("Reloading Imaging settings…", RefreshStatusAndConfigurationAsync);
            if (_configurationNeedsRefresh || _captureNeedsReview) return false;
        }
        if (_configurationSnapshot is { } snapshot) ApplyConfiguration(snapshot);
        if (_sharing is { } setup) { setup.Discard(); PopulateSharing(); }
        _settingsSaveResult = null;
        SetControlsForOperation(false);
        return true;
    }

    private static TextBox? NumberEditor(DependencyObject root)
    {
        for (int i = 0; i < VisualTreeHelper.GetChildrenCount(root); i++)
        {
            var child = VisualTreeHelper.GetChild(root, i);
            if (child is TextBox editor) return editor;
            if (NumberEditor(child) is { } nested) return nested;
        }
        return null;
    }

    private void TrackNumberEditor(NumberBox box, Action changed)
    {
        // NumberBox.Text is committed on Enter/focus loss. Observe the actual
        // template editor so Save enables while typing, including invalid drafts.
        box.ValidationMode = NumberBoxValidationMode.Disabled;
        box.Loaded += (_, _) => {
            if (NumberEditor(box) is { } editor && _trackedNumberEditors.Add(editor))
                editor.TextChanged += (_, _) => { _settingsSaveResult = null; changed(); };
        };
    }

    private static string LiveNumberText(NumberBox box) => NumberEditor(box)?.Text
        ?? (double.IsNaN(box.Value) ? "" : box.Value.ToString("R", CultureInfo.CurrentCulture));

    private static void ResetNumberEditors(IEnumerable<NumberBox> boxes)
    {
        // Setting Value to its existing value doesn't replace an invalid draft.
        foreach (var box in boxes)
            box.Text = double.IsNaN(box.Value) ? "" : box.Value.ToString("R", CultureInfo.CurrentCulture);
    }

    private static double ReadNumber(NumberBox box)
    {
        try { return SettingsFormValues.ReadNumberText(LiveNumberText(box), CultureInfo.CurrentCulture); }
        catch (InvalidOperationException) { throw new UserInputException($"{box.Header}: enter a valid number."); }
    }
}
