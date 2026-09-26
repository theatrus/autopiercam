namespace AutoPierCam.Viewer;

// Keep connection refreshes separate from the editable draft. Revisions are
// concurrency tokens, never user-facing version numbers.
internal sealed class SharingSetupState(SharingStatus status)
{
    internal SharingStatus Status { get; private set; } = status;
    internal SharingPreferences Draft { get; set; } = status.Preferences;
    internal ulong ExpectedRevision { get; private set; } = status.Revision;
    private SharingPreferences _baseline = status.Preferences;
    internal bool IsDirty => Draft != _baseline;
    internal bool NeedsReview => ExpectedRevision != Status.Revision;
    internal bool HasChatOverrides => Status.ActiveTriggers is { } rules && rules != new SharingTriggerRules {
        IntervalMinutes = Status.Preferences.IntervalMinutes,
        SceneChanges = Status.Preferences.SceneChanges,
        DayNight = Status.Preferences.DayNight,
        TelescopeEvents = Status.Preferences.TelescopeEvents,
        BurstCount = Status.Preferences.BurstCount,
        SpacingSeconds = Status.Preferences.SpacingSeconds
    };

    internal void Refresh(SharingStatus latest, bool preserveDraft = false)
    {
        if (!preserveDraft && !IsDirty && !NeedsReview) Accept(latest);
        else Status = latest;
    }

    internal void Accept(SharingStatus latest)
    {
        Status = latest;
        Draft = _baseline = latest.Preferences;
        ExpectedRevision = latest.Revision;
    }

    internal void KeepEdits()
    {
        _baseline = Status.Preferences;
        ExpectedRevision = Status.Revision;
    }

    internal void Discard() => Accept(Status);

    internal void Stopped(SharingStatus before, SharingStatus latest, bool preserveDraft = false)
    {
        bool keep = IsDirty || preserveDraft;
        ulong previousRevision = ExpectedRevision;
        var pending = keep ? Draft with { Enabled = false } : latest.Preferences;
        Accept(latest);
        Draft = pending;
        // Stopping is not approval to overwrite an intervening external edit.
        if (keep && before.Revision != previousRevision) ExpectedRevision = previousRevision;
    }

    internal SharingPreferences ForSave()
    {
        if (NeedsReview)
            throw new InvalidOperationException("Settings changed elsewhere. Review them before saving.");
        if (Draft.Enabled && Status.DeviceId is null)
            throw new InvalidOperationException("Pair this camera before enabling image sharing.");
        return Draft;
    }

    internal static string PairingCode(string value)
    {
        string token = value.Trim();
        if (!token.StartsWith("csdp_", StringComparison.Ordinal) || token.Length <= 5 || token.Length > 512)
            throw new InvalidOperationException("Paste a one-use device pairing code from Observatory devices in the Hub.");
        return token;
    }
}
