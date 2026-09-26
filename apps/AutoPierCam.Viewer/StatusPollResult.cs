namespace AutoPierCam.Viewer;

// Capture status and optional sharing status are independent observations.
// Not polling a hidden sharing section must never disconnect the capture UI.
internal sealed record StatusPollResult(AgentStatus? Agent, SharingStatus? Sharing)
{
    internal void Apply(Action<AgentStatus> agentAvailable, Action agentUnavailable,
        Action<SharingStatus> sharingAvailable)
    {
        if (Agent is { } agent) agentAvailable(agent);
        else agentUnavailable();
        if (Sharing is { } sharing) sharingAvailable(sharing);
    }
}
