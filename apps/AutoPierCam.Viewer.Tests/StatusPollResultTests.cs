using AutoPierCam.Viewer;
using Xunit;

public sealed class StatusPollResultTests
{
    [Fact]
    public void HiddenOrUnavailableSharingNeverDisablesCaptureSave()
    {
        bool available = false;
        bool dirty = true;
        var agent = new AgentStatus { State = "capturing" };
        int unavailableCalls = 0;
        // Repeated polls with the sharing section closed (or its request failed).
        for (int i = 0; i < 4; i++)
        {
            new StatusPollResult(agent, null).Apply(
                value => { Assert.Same(agent, value); available = true; },
                () => { unavailableCalls++; available = false; },
                _ => Assert.Fail("No sharing result was polled"));
            Assert.True(available && dirty);
        }
        Assert.Equal(0, unavailableCalls);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void AgentFailureAndRecoveryAreIndependentOfSharing(bool hasSharing)
    {
        bool available = true;
        var sharing = hasSharing ? new SharingStatus { DeviceId = 42 } : null;
        int sharingCalls = 0;
        void ApplySharing(SharingStatus value) { Assert.Same(sharing, value); sharingCalls++; }
        new StatusPollResult(null, sharing).Apply(_ => available = true, () => available = false, ApplySharing);
        Assert.False(available);
        new StatusPollResult(new AgentStatus { State = "capturing" }, sharing)
            .Apply(_ => available = true, () => available = false, ApplySharing);
        Assert.True(available);
        Assert.Equal(hasSharing ? 2 : 0, sharingCalls);
    }
}
