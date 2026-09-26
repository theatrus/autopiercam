using AutoPierCam.Viewer;
using Xunit;

public sealed class SharingSetupTests
{
    [Fact]
    public void IntervalEditPreservesThresholdAndOtherCollapsedNumericFields()
    {
        var setup = new SharingSetupState(Status);
        var saved = setup.Draft;
        setup.Draft = saved with {
            IntervalMinutes = SharingSetupState.WholeNumber("25", 0, 1440, "Interval"),
            SceneThresholdPercent = (byte)SharingSetupState.WholeNumber(saved.SceneThresholdPercent.ToString(), 5, 80, "Threshold"),
            BurstCount = (byte)SharingSetupState.WholeNumber(saved.BurstCount.ToString(), 1, 3, "Burst"),
            SpacingSeconds = SharingSetupState.WholeNumber(saved.SpacingSeconds.ToString(), 60, 600, "Spacing")
        };
        Assert.Equal(25, setup.ForSave().IntervalMinutes);
        Assert.Equal(saved.SceneThresholdPercent, setup.ForSave().SceneThresholdPercent);
        Assert.Equal(saved.BurstCount, setup.ForSave().BurstCount);
        Assert.Equal(saved.SpacingSeconds, setup.ForSave().SpacingSeconds);
    }

    [Theory]
    [InlineData("")]
    [InlineData("NaN")]
    [InlineData("20.5")]
    [InlineData("4")]
    [InlineData("81")]
    [InlineData("65536")]
    public void InvalidNumericEditsAreRejectedAndCorrectionWorksWithoutReopening(string text)
    {
        Assert.Throws<InvalidOperationException>(() => SharingSetupState.WholeNumber(text, 5, 80, "Threshold"));
        Assert.Equal(20, SharingSetupState.WholeNumber(" 20 ", 5, 80, "Threshold"));
    }

    private static SharingStatus Status => new() {
        Revision = 12, DeviceId = 42, Connection = "Disabled",
        Preferences = new() { HubOrigin = "https://hub.example.test", Snapshots = true, IntervalMinutes = 10 }
    };

    [Fact]
    public void RefreshPreservesDraftAndDoesNotTurnItIntoSavedSettings()
    {
        var setup = new SharingSetupState(Status);
        var draft = setup.Draft with { SceneChanges = true, IntervalMinutes = 25, BurstCount = 3 };
        setup.Draft = draft;
        setup.Refresh(Status with { Connection = "Connected", LastDeliveryUnixMs = 123 });
        Assert.Equal(draft, setup.Draft);
        Assert.True(setup.IsDirty);
        Assert.False(setup.NeedsReview);
        Assert.Equal("Connected", setup.Status.Connection);
    }

    [Fact]
    public void ExternalChangeRequiresExplicitReviewBeforeReplacingSettings()
    {
        var setup = new SharingSetupState(Status);
        setup.Draft = setup.Draft with { IntervalMinutes = 20 };
        var latest = Status with { Revision = 13, Preferences = Status.Preferences with { Snapshots = false } };
        setup.Refresh(latest);
        Assert.True(setup.NeedsReview);
        Assert.Equal(12UL, setup.ExpectedRevision);
        Assert.Throws<InvalidOperationException>(() => setup.ForSave());
        setup.KeepEdits();
        Assert.Equal(13UL, setup.ExpectedRevision);
        Assert.False(setup.NeedsReview);
        Assert.True(setup.ForSave().Snapshots);
        Assert.Equal(20, setup.ForSave().IntervalMinutes);
    }

    [Fact]
    public void DiscardLoadsLatestSettingsAndClearsConflict()
    {
        var setup = new SharingSetupState(Status);
        setup.Draft = setup.Draft with { IntervalMinutes = 20 };
        var latest = Status with { Revision = 13, Preferences = Status.Preferences with { Snapshots = false } };
        setup.Refresh(latest);
        setup.Discard();
        Assert.False(setup.IsDirty);
        Assert.False(setup.NeedsReview);
        Assert.Equal(latest.Preferences, setup.Draft);
    }

    [Fact]
    public void CleanRefreshLoadsSavedPreferences()
    {
        var setup = new SharingSetupState(Status);
        var latest = Status with { Revision = 13, Preferences = Status.Preferences with { IntervalMinutes = 30 } };
        setup.Refresh(latest);
        Assert.Equal(latest.Preferences, setup.Draft);
        Assert.Equal(13UL, setup.ExpectedRevision);
        Assert.False(setup.IsDirty);
    }

    [Fact]
    public void InvalidNumericTextStillFencesARefreshAgainstConcurrentChanges()
    {
        var setup = new SharingSetupState(Status);
        setup.Refresh(Status with { Revision = 13 }, preserveDraft: true);
        Assert.True(setup.NeedsReview);
        Assert.Equal(12UL, setup.ExpectedRevision);
    }

    [Fact]
    public void StopForcesMasterOffButKeepsOtherUnsavedEdits()
    {
        var initial = Status with { Preferences = Status.Preferences with { Enabled = true } };
        var setup = new SharingSetupState(initial);
        setup.Draft = setup.Draft with { SceneChanges = true, IntervalMinutes = 25, ChatConfiguration = true };
        setup.Stopped(initial, Status with { Revision = 13 });
        Assert.False(setup.Draft.Enabled);
        Assert.True(setup.Draft.SceneChanges);
        Assert.True(setup.Draft.ChatConfiguration);
        Assert.Equal(25, setup.Draft.IntervalMinutes);
        Assert.True(setup.IsDirty);
        Assert.False(setup.NeedsReview);
        Assert.Equal(13UL, setup.ExpectedRevision);
    }

    [Fact]
    public void StopWithoutEditsDoesNotRestoreOldPreferencesFromBeforeAnExternalChange()
    {
        var setup = new SharingSetupState(Status);
        var latest = Status with { Revision = 13, Preferences = Status.Preferences with { IntervalMinutes = 99 } };
        setup.Stopped(latest, latest with { Revision = 14 });
        Assert.Equal(99, setup.Draft.IntervalMinutes);
        Assert.False(setup.IsDirty);
        Assert.False(setup.NeedsReview);
    }

    [Fact]
    public void StopDoesNotAcknowledgeConcurrentEditsOnBehalfOfTheOperator()
    {
        var setup = new SharingSetupState(Status);
        setup.Draft = setup.Draft with { IntervalMinutes = 20 };
        var latest = Status with { Revision = 13, Preferences = Status.Preferences with { Snapshots = false } };
        setup.Stopped(latest, latest with { Revision = 14 });
        Assert.False(setup.Draft.Enabled);
        Assert.Equal(20, setup.Draft.IntervalMinutes);
        Assert.True(setup.NeedsReview);
        Assert.Throws<InvalidOperationException>(() => setup.ForSave());
    }

    [Fact]
    public void ChatOverridesCanBeResetWithoutChangingLocalChoices()
    {
        var setup = new SharingSetupState(Status with { ActiveTriggers = new() { IntervalMinutes = 60, BurstCount = 1, SpacingSeconds = 60 } });
        Assert.False(setup.IsDirty);
        Assert.True(setup.HasChatOverrides);
        setup.Refresh(Status with { ActiveTriggers = new() { IntervalMinutes = 10, BurstCount = 1, SpacingSeconds = 60 } });
        Assert.False(setup.HasChatOverrides);
    }

    [Fact]
    public void FailedPairRefreshKeepsChoicesAndUsesNewRevisionWithoutRetry()
    {
        var initial = Status with { DeviceId = null };
        var setup = new SharingSetupState(initial);
        setup.Refresh(initial with { Revision = 13 }); // agent persisted sharing-off before HTTP failure
        Assert.Equal(initial.Preferences, setup.Draft);
        Assert.False(setup.Draft.Enabled);
        Assert.Equal(13UL, setup.ExpectedRevision);
        Assert.False(setup.NeedsReview);
    }

    [Fact]
    public void PairingIsSeparateFromEnabling()
    {
        var setup = new SharingSetupState(Status with { DeviceId = null });
        setup.Draft = setup.Draft with { Enabled = true };
        Assert.Throws<InvalidOperationException>(() => setup.ForSave());
        setup.Accept(Status with { Revision = 14 });
        Assert.True(setup.Draft.Snapshots);
        Assert.False(setup.Draft.Enabled);
        setup.Draft = setup.Draft with { Enabled = true };
        Assert.True(setup.ForSave().Enabled);
    }

    [Theory]
    [InlineData("")]
    [InlineData("   ")]
    [InlineData("csdc_credential")]
    [InlineData("csdp_")]
    public void InvalidCodesAreRejectedBeforeAnySettingsMutation(string code)
    {
        Assert.Throws<InvalidOperationException>(() => SharingSetupState.PairingCode(code));
    }

    [Fact]
    public void PastedCodeTrimsSurroundingWhitespaceAndEnforcesLength()
    {
        Assert.Equal("csdp_test", SharingSetupState.PairingCode(" csdp_test\r\n"));
        Assert.Throws<InvalidOperationException>(() => SharingSetupState.PairingCode("csdp_" + new string('x', 508)));
    }
}
