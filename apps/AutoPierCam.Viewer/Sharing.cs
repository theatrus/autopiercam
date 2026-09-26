using System.Globalization;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private async void SharingButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Loading Chatstronomy sharing…", ShowSharingAsync);
    }

    private async Task ShowSharingAsync(CancellationToken cancellationToken)
    {
        var setup = new SharingSetupState(await _agentClient.GetSharingAsync(cancellationToken));
        static TextBlock Note(string text) => new() { Text = text, TextWrapping = TextWrapping.Wrap };
        static TextBlock Heading(string text) => new() { Text = text, FontSize = 18 };
        static StackPanel Group(params UIElement[] elements)
        {
            var panel = new StackPanel { Spacing = 10 };
            foreach (var element in elements) panel.Children.Add(element);
            return panel;
        }
        var connection = Note("");
        var origin = new TextBox { Header = "Hub HTTPS origin", PlaceholderText = "https://your-chatstronomy-hub" };
        var code = new PasswordBox { Header = "One-use device pairing code", PlaceholderText = "Paste code from the Hub" };
        var pair = new Button { Content = "Pair camera" };
        var pairingFields = Group(
            Note("In the Hub, open Observatory devices, add a pier camera, select its destination channels, and generate a pairing code."),
            origin, code, pair, Note("Pairing keeps your settings below. It does not start sharing."));
        var header = Group(Heading("1. Pair with your Hub"), connection, pairingFields);

        var enabled = new ToggleSwitch { Header = "Enable image sharing" };
        var snapshots = new ToggleSwitch { Header = "Allow Snapshot now requests" };
        var interval = new TextBox { Header = "Send an image every (minutes; 0 = off)" };
        var scenes = new ToggleSwitch { Header = "Send when the scene changes" };
        var dayNight = new ToggleSwitch { Header = "Send on day / night transitions" };
        var telescopeEvents = new ToggleSwitch { Header = "Send on slew / sequence events" };
        // Text is the single source of truth, including before collapsed content
        // is templated. NumberBox.Value can be populated while Text is still empty.
        var threshold = new TextBox { Header = "Changed area threshold (%; 5–80)" };
        var burst = new TextBox { Header = "Images per scene / telescope event (1–3)" };
        var spacing = new TextBox { Header = "Seconds between event images (60–600)" };
        var chatConfiguration = new ToggleSwitch { Header = "Allow the camera owner to adjust triggers in chat" };
        var events = new Expander {
            Header = "Scene and telescope events", HorizontalAlignment = HorizontalAlignment.Stretch,
            Content = Group(scenes, threshold, dayNight, telescopeEvents, burst, spacing,
                Note("Scene changes are not person or threat detection. Telescope events require the same owner and a shared Hub channel. Bursts wait for distinct completed frames; busy events are coalesced."))
        };
        var chat = new Expander {
            Header = "Chat control", HorizontalAlignment = HorizontalAlignment.Stretch,
            Content = Group(chatConfiguration, Note("Chat can turn permitted triggers off, slow sends, or reduce bursts. It cannot enable a source you disabled or exceed your limits. Saving settings resets chat overrides."))
        };
        var details = Note("");
        var forgetConsent = new CheckBox { Content = "Remove pairing and clear sharing permissions" };
        var forget = new Button { Content = "Forget pairing" };
        var management = new Expander {
            Header = "Connection details / change Hub", HorizontalAlignment = HorizontalAlignment.Stretch,
            Content = Group(details, Note("To change Hub, stop and forget this pairing first. This clears saved permissions. Revoke the credential in the Hub too."), forgetConsent, forget)
        };
        var settings = Group(Heading("2. Choose what to share"),
            Note("The full preview goes to the channels selected in the Hub. Snapshot now uses the latest completed frame (up to 120 seconds old); it does not change exposure."),
            enabled, snapshots, interval, events, chat, management,
            Note("Stop sharing disconnects and discards queued images. Messages already posted cannot be recalled. Retries may duplicate a post after a Hub crash."));
        var feedback = Note("");
        var edits = Note("");
        var reload = new Button { Content = "Refresh status" };
        var discard = new Button { Content = "Discard changes" };
        var keep = new Button { Content = "Keep my edits" };
        var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        actions.Children.Add(reload);
        actions.Children.Add(discard);
        actions.Children.Add(keep);
        var footer = Group(edits, feedback, actions);
        var body = new Grid { Width = 460, MaxHeight = 620, RowSpacing = 12 };
        body.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        body.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        body.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        var scroll = new ScrollViewer { Content = settings, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        Grid.SetRow(scroll, 1);
        Grid.SetRow(footer, 2);
        body.Children.Add(header);
        body.Children.Add(scroll);
        body.Children.Add(footer);
        var dialog = new ContentDialog {
            XamlRoot = Content.XamlRoot, Title = "Chatstronomy image sharing", Content = body,
            PrimaryButtonText = "Save settings", SecondaryButtonText = "Stop sharing",
            CloseButtonText = "Close", DefaultButton = ContentDialogButton.Close
        };
        bool busy = false, loading = false, invalidDraft = false, statusUnknown = false;
        bool HasEdits() => setup.IsDirty || invalidDraft;
        static ushort Number(TextBox box, ushort minimum, ushort maximum) =>
            SharingSetupState.WholeNumber(box.Text, minimum, maximum, box.Header.ToString()!);
        SharingPreferences Inputs() => new() {
            HubOrigin = origin.Text.Trim(), Enabled = enabled.IsOn,
            Snapshots = snapshots.IsOn, SceneChanges = scenes.IsOn, DayNight = dayNight.IsOn,
            SceneThresholdPercent = checked((byte)Number(threshold, 5, 80)), IntervalMinutes = Number(interval, 0, 1440),
            TelescopeEvents = telescopeEvents.IsOn, ChatConfiguration = chatConfiguration.IsOn,
            BurstCount = checked((byte)Number(burst, 1, 3)), SpacingSeconds = Number(spacing, 60, 600)
        };
        void Render()
        {
            bool paired = setup.Status.DeviceId is not null;
            pairingFields.Visibility = paired ? Visibility.Collapsed : Visibility.Visible;
            connection.Text = paired
                ? $"Paired · {(setup.Status.Preferences.Enabled ? setup.Status.Connection : "Sharing off")}" : "Not paired · No images are shared";
            foreach (var control in new Control[] { origin, code, snapshots, interval, scenes, dayNight, telescopeEvents, threshold, burst, spacing, chatConfiguration, events, chat, management })
                control.IsEnabled = !busy;
            enabled.IsEnabled = paired && !busy && !statusUnknown;
            pair.IsEnabled = !busy && !statusUnknown && !setup.NeedsReview && !string.IsNullOrWhiteSpace(origin.Text) && !string.IsNullOrWhiteSpace(code.Password);
            forgetConsent.IsEnabled = paired && !busy;
            forget.IsEnabled = paired && !busy && !statusUnknown && forgetConsent.IsChecked == true;
            reload.IsEnabled = !busy;
            discard.IsEnabled = !busy && (HasEdits() || setup.NeedsReview);
            keep.Visibility = setup.NeedsReview ? Visibility.Visible : Visibility.Collapsed;
            keep.IsEnabled = !busy && !statusUnknown;
            dialog.IsPrimaryButtonEnabled = !busy && !statusUnknown && !setup.NeedsReview && (HasEdits() || setup.HasChatOverrides);
            dialog.PrimaryButtonText = enabled.IsOn && !setup.Status.Preferences.Enabled ? "Save and enable sharing" : "Save settings";
            dialog.IsSecondaryButtonEnabled = !busy && (setup.Status.Preferences.Enabled || statusUnknown);
            edits.Text = statusUnknown ? "Agent status could not be confirmed. Refresh before saving or pairing."
                : setup.NeedsReview ? "Settings changed in the agent or chat. Discard your edits to load them, or keep your edits to replace them on the next save."
                : HasEdits() ? "Unsaved changes · Save below to apply."
                : setup.HasChatOverrides ? "Chat overrides are active. Save settings to restore your local trigger choices."
                : paired ? "Settings saved · Sharing starts only when enabled and saved." : "Pair above, then enable sharing and save below.";
            var state = setup.Status;
            details.Text = $"Hub: {state.Preferences.HubOrigin}\nDevice: {state.DeviceId?.ToString() ?? "not paired"}\nInstallation: {state.InstallationId}\nConnection: {state.Connection}";
            if (state.LastDeliveryUnixMs is ulong ms && ms <= 253402300799999)
                details.Text += $"\nLast delivery: {DateTimeOffset.FromUnixTimeMilliseconds((long)ms).ToLocalTime():g}";
            if (state.ActiveTriggers is { } active)
                details.Text += $"\n{(state.Preferences.Enabled ? "Active" : "Configured")} triggers: every {active.IntervalMinutes} min (0 = off); scene {(active.SceneChanges ? "on" : "off")}; day/night {(active.DayNight ? "on" : "off")}; telescope {(active.TelescopeEvents ? "on" : "off")}; {active.BurstCount} images, {active.SpacingSeconds}s apart";
        }
        void Populate()
        {
            loading = true;
            var p = setup.Draft;
            origin.Text = p.HubOrigin;
            enabled.IsOn = p.Enabled;
            snapshots.IsOn = p.Snapshots;
            scenes.IsOn = p.SceneChanges;
            dayNight.IsOn = p.DayNight;
            threshold.Text = p.SceneThresholdPercent.ToString(CultureInfo.CurrentCulture);
            interval.Text = p.IntervalMinutes.ToString(CultureInfo.CurrentCulture);
            telescopeEvents.IsOn = p.TelescopeEvents;
            burst.Text = p.BurstCount.ToString(CultureInfo.CurrentCulture);
            spacing.Text = p.SpacingSeconds.ToString(CultureInfo.CurrentCulture);
            chatConfiguration.IsOn = p.ChatConfiguration;
            invalidDraft = false;
            loading = false;
            Render();
        }
        void Changed()
        {
            if (loading || busy) return;
            try { setup.Draft = Inputs(); invalidDraft = false; }
            catch (InvalidOperationException) { invalidDraft = true; }
            Render();
        }
        async Task Refresh()
        {
            bool preserve = invalidDraft;
            setup.Refresh(await _agentClient.GetSharingAsync(cancellationToken), preserve);
            statusUnknown = false;
            if (!preserve) Populate();
        }
        async Task Operate(Func<Task> action, bool pairing = false)
        {
            if (busy) return;
            busy = true;
            Render();
            feedback.Text = "";
            try { await action(); }
            catch (Exception error) {
                feedback.Text = error is OperationCanceledException ? "Operation cancelled." : error.Message;
                // Pairing can persist a revision even if the HTTP request fails.
                // Reconcile it without discarding the operator's draft or retrying the code.
                try { await Refresh(); }
                catch { statusUnknown = true; }
                if (pairing) feedback.Text += " A code may have been consumed; get a new code from the Hub before retrying.";
            }
            finally { busy = false; Render(); }
        }
        origin.TextChanged += (_, _) => Changed();
        foreach (var toggle in new[] { enabled, snapshots, scenes, dayNight, telescopeEvents, chatConfiguration })
            toggle.Toggled += (_, _) => Changed();
        foreach (var number in new[] { threshold, interval, burst, spacing })
            number.TextChanged += (_, _) => Changed();
        code.PasswordChanged += (_, _) => Render();
        forgetConsent.Checked += (_, _) => Render();
        forgetConsent.Unchecked += (_, _) => Render();
        pair.Click += async (_, _) => {
            string token;
            try { token = SharingSetupState.PairingCode(code.Password); setup.Draft = Inputs(); }
            catch (InvalidOperationException error) { feedback.Text = error.Message; return; }
            code.Password = "";
            await Operate(async () => {
                setup.Accept(await _agentClient.ConfigureSharingAsync(setup.ExpectedRevision, setup.Draft with { Enabled = false }, cancellationToken));
                Populate();
                setup.Accept(await _agentClient.PairSharingAsync(setup.ExpectedRevision, token, cancellationToken));
                Populate();
                feedback.Text = "Paired. Your choices are saved. Turn on Enable image sharing and save when ready.";
            }, pairing: true);
            if (setup.Status.DeviceId is not null) enabled.Focus(FocusState.Programmatic);
        };
        forget.Click += async (_, _) => await Operate(async () => {
            setup.Accept(await _agentClient.ForgetSharingAsync(setup.Status.Revision, cancellationToken));
            code.Password = "";
            forgetConsent.IsChecked = false;
            Populate();
            feedback.Text = "Pairing and permissions cleared. Revoke the device credential in the Hub too.";
        });
        reload.Click += async (_, _) => await Operate(Refresh);
        discard.Click += (_, _) => { setup.Discard(); Populate(); feedback.Text = "Unsaved changes discarded."; };
        keep.Click += (_, _) => { setup.KeepEdits(); Render(); feedback.Text = "Your edits are kept. Saving will replace the agent settings and reset chat overrides."; };
        dialog.PrimaryButtonClick += async (_, args) => {
            args.Cancel = true;
            var deferral = args.GetDeferral();
            try { await Operate(async () => {
                // Always validate the current text, never a cached load-time error.
                setup.Draft = Inputs();
                invalidDraft = false;
                setup.Accept(await _agentClient.ConfigureSharingAsync(setup.ExpectedRevision, setup.ForSave(), cancellationToken));
                Populate();
                feedback.Text = setup.Status.Preferences.Enabled ? "Settings saved. Connecting to the Hub; refresh to check delivery." : "Settings saved. Sharing is off.";
            }); } finally { deferral.Complete(); }
        };
        dialog.SecondaryButtonClick += async (_, args) => {
            args.Cancel = true;
            var deferral = args.GetDeferral();
            try { await Operate(async () => {
                // Stop immediately using saved permissions, not the pending draft.
                var latest = await _agentClient.GetSharingAsync(cancellationToken);
                setup.Stopped(latest, await _agentClient.ConfigureSharingAsync(latest.Revision, latest.Preferences with { Enabled = false }, cancellationToken), invalidDraft);
                statusUnknown = false;
                if (!invalidDraft) Populate();
                else {
                    loading = true;
                    enabled.IsOn = false;
                    loading = false;
                }
                feedback.Text = "Sharing stopped; queued images discarded. Your other edits are still here.";
            }); } finally { deferral.Complete(); }
        };
        dialog.Closing += (_, args) => {
            if (busy || HasEdits()) {
                args.Cancel = true;
                if (!busy) feedback.Text = "Save or discard your changes before closing.";
            }
        };
        Populate();
        await dialog.ShowAsync();
        code.Password = "";
    }
}
