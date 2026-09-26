using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text.Json;
using System.Text.Json.Nodes;
using AutoPierCam.Viewer;
using Xunit;

public sealed class AgentPipeClientTests
{
    [Fact]
    public async Task SaveAcceptsLiveReloadWithoutRestart()
    {
        var configuration = JsonSerializer.Deserialize<AgentConfiguration>(
            await File.ReadAllTextAsync(Path.Combine(AppContext.BaseDirectory, "config-default.json")))!;
        await WithResponse("config.replace", new { revision = 1UL, saved = true, restart_scheduled = false },
            async client => {
                var result = await client.ReplaceConfigurationAsync(1, configuration);
                Assert.True(result.Saved);
                Assert.False(result.RestartScheduled);
            });
    }
    private static async Task WithResponse(string method, object result, Func<AgentPipeClient, Task> assertion, Action<JsonElement>? inspectRequest = null)
    {
        string name = $"apc-test-{Guid.NewGuid():N}";
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        await using var server = new NamedPipeServerStream(name, PipeDirection.InOut, 1,
            PipeTransmissionMode.Byte, PipeOptions.Asynchronous);
        Task serve = Task.Run(async () =>
        {
            await server.WaitForConnectionAsync(timeout.Token);
            byte[] prefix = new byte[4];
            await server.ReadExactlyAsync(prefix, timeout.Token);
            byte[] body = new byte[BinaryPrimitives.ReadInt32LittleEndian(prefix)];
            await server.ReadExactlyAsync(body, timeout.Token);
            using var request = JsonDocument.Parse(body);
            Assert.Equal(method, request.RootElement.GetProperty("method").GetString());
            inspectRequest?.Invoke(request.RootElement);
            byte[] response = JsonSerializer.SerializeToUtf8Bytes(new {
                version = 1, request_id = request.RootElement.GetProperty("request_id").GetString(), result });
            BinaryPrimitives.WriteInt32LittleEndian(prefix, response.Length);
            await server.WriteAsync(prefix, timeout.Token);
            await server.WriteAsync(response, timeout.Token);
            await server.FlushAsync(timeout.Token);
        }, timeout.Token);
        await using var client = new AgentPipeClient(name, TimeSpan.FromSeconds(3), TimeSpan.FromSeconds(3));
        await assertion(client);
        await serve;
    }

    [Fact]
    public async Task FullStatusReadsCarryLiveStateCountsAndExposureFromTheSameResponse()
    {
        foreach (string state in new[] { "starting", "capturing", "paused", "faulted", "starting", "capturing" })
        {
            bool settling = state == "starting";
            await WithResponse("status.get", new {
                state, frames_captured = 60, frames_saved = 3,
                camera = new { id = 0, name = "ZWO ASI662MC" },
                capabilities = new[] { "exposure.progress" },
                exposure = new { session_generation = 9, settling, exposure_us = 8765000,
                    gain = 300, max_exposure_us = 60000000, settling_frames = 60,
                    settling_min_frames = 6, wait_elapsed_ms = 0, frame_timeout_ms = 22530 }
            }, async client => {
                var status = await client.GetStatusAsync();
                Assert.Equal(state, status.State);
                Assert.Equal(state, status.Progress!.State);
                Assert.Equal(60UL, status.FramesCaptured);
                Assert.Equal(3UL, status.FramesSaved);
                Assert.Equal(8765000, status.Progress.Exposure!.ExposureUs);
                Assert.Equal(settling, status.Progress.Exposure.Settling);
                if (settling) Assert.Equal("Stabilizing exposure", status.DisplayState);
                else if (state == "capturing") Assert.Equal("Capturing", status.DisplayState);
                else if (state == "paused") Assert.StartsWith("Recording paused", status.DisplayState);
            });
        }
    }

    [Fact]
    public async Task FullStatusSupportsOldAgentsWithoutOptionalExposure()
    {
        await WithResponse("status.get", new { state = "capturing", frames_captured = 12, frames_saved = 2 }, async client => {
            var status = await client.GetStatusAsync();
            Assert.Equal("Capturing", status.DisplayState);
            Assert.NotNull(status.Progress);
            Assert.Null(status.Progress.Exposure);
        });
    }

    [Fact]
    public async Task SharingConfigureAndPairKeepChoicesAndSendExactRevision()
    {
        var preferences = new SharingPreferences {
            HubOrigin = "https://hub.example.test", Snapshots = true, SceneChanges = true,
            DayNight = true, SceneThresholdPercent = 35, IntervalMinutes = 15,
            TelescopeEvents = true, ChatConfiguration = true, BurstCount = 3, SpacingSeconds = 120
        };
        var saved = new SharingStatus { Revision = 12, Preferences = preferences };
        await WithResponse("sharing.configure", saved, async client => {
            var response = await client.ConfigureSharingAsync(11, preferences);
            Assert.Equal(preferences, response.Preferences);
            Assert.False(response.Preferences.Enabled);
            Assert.Equal(12UL, response.Revision);
        }, request => {
            var payload = request.GetProperty("payload");
            Assert.Equal(11UL, payload.GetProperty("expected_revision").GetUInt64());
            Assert.Equal(preferences, payload.GetProperty("preferences").Deserialize<SharingPreferences>());
        });
        await WithResponse("sharing.pair", saved with { Revision = 14, DeviceId = 42 }, async client => {
            var response = await client.PairSharingAsync(12, "csdp_synthetic_test");
            Assert.Equal(preferences, response.Preferences);
            Assert.False(response.Preferences.Enabled);
            Assert.Equal(42, response.DeviceId);
        }, request => {
            var payload = request.GetProperty("payload");
            Assert.Equal(12UL, payload.GetProperty("expected_revision").GetUInt64());
            Assert.Equal("csdp_synthetic_test", payload.GetProperty("pairing_token").GetString());
        });
    }

    [Fact]
    public async Task DefaultConfigurationRoundTripsThroughRealClientWithExplicitNullLimits()
    {
        var fixture = JsonNode.Parse(await File.ReadAllTextAsync(Path.Combine(AppContext.BaseDirectory, "config-default.json")))!;
        AgentConfigurationSnapshot? snapshot = null;
        await WithResponse("config.get", new { revision = 12509826116217928070UL, config = fixture },
            async client => snapshot = await client.GetConfigurationAsync());
        var updated = snapshot!.Config with { Camera = snapshot.Config.Camera with { CameraId = 7, NameContains = "ASI676MC" } };
        fixture["camera"]!["camera_id"] = 7;
        fixture["camera"]!["name_contains"] = "ASI676MC";
        await WithResponse("config.replace", new { revision = 1UL, saved = true, restart_scheduled = true },
            client => client.ReplaceConfigurationAsync(snapshot.Revision, updated), request => {
                var payload = request.GetProperty("payload");
                Assert.Equal(snapshot.Revision, payload.GetProperty("expected_revision").GetUInt64());
                Assert.True(JsonNode.DeepEquals(fixture, JsonNode.Parse(payload.GetProperty("config").GetRawText())));
            });
    }

    [Theory]
    [InlineData(1000000, 2000000)]
    [InlineData(1000000, null)]
    [InlineData(null, 2000000)]
    public async Task ExistingLimitsCanBeExplicitlyDisabled(int? maxBytes, int? freeBytes)
    {
        var fixture = JsonNode.Parse(await File.ReadAllTextAsync(Path.Combine(AppContext.BaseDirectory, "config-default.json")))!;
        fixture["capture"]!["retention_max_bytes"] = maxBytes;
        fixture["capture"]!["retention_min_free_bytes"] = freeBytes;
        AgentConfigurationSnapshot? snapshot = null;
        await WithResponse("config.get", new { revision = 1, config = fixture },
            async client => snapshot = await client.GetConfigurationAsync());
        Assert.Equal((ulong?)maxBytes, snapshot!.Config.Capture.RetentionMaxBytes);
        Assert.Equal((ulong?)freeBytes, snapshot.Config.Capture.RetentionMinFreeBytes);
        var updated = snapshot.Config with { Capture = snapshot.Config.Capture with { RetentionMaxBytes = null, RetentionMinFreeBytes = null } };
        fixture["capture"]!["retention_max_bytes"] = null;
        fixture["capture"]!["retention_min_free_bytes"] = null;
        await WithResponse("config.replace", new { revision = 2, saved = true, restart_scheduled = true },
            client => client.ReplaceConfigurationAsync(1, updated), request =>
                Assert.True(JsonNode.DeepEquals(fixture, JsonNode.Parse(request.GetProperty("payload").GetProperty("config").GetRawText()))));
    }

    [Theory]
    [InlineData(false, false)]
    [InlineData(true, true)]
    [InlineData(true, false)]
    [InlineData(false, true)]
    public async Task RetentionPresenceIsPreservedIncludingLegacyAgents(bool maxPresent, bool freePresent)
    {
        var fixture = JsonNode.Parse(await File.ReadAllTextAsync(Path.Combine(AppContext.BaseDirectory, "config-default.json")))!;
        if (!maxPresent) fixture["capture"]!.AsObject().Remove("retention_max_bytes");
        if (!freePresent) fixture["capture"]!.AsObject().Remove("retention_min_free_bytes");
        AgentConfigurationSnapshot? snapshot = null;
        await WithResponse("config.get", new { revision = 1, config = fixture },
            async client => snapshot = await client.GetConfigurationAsync());
        // Mirrors the Viewer's with-expression when controls are disabled.
        var updated = snapshot!.Config with { Capture = snapshot.Config.Capture with {
            RetentionMaxBytes = null, RetentionMinFreeBytes = null,
        } };
        await WithResponse("config.replace", new { revision = 2, saved = true, restart_scheduled = true },
            client => client.ReplaceConfigurationAsync(1, updated), request =>
                Assert.True(JsonNode.DeepEquals(fixture, JsonNode.Parse(request.GetProperty("payload").GetProperty("config").GetRawText()))));
    }

    [Fact]
    public async Task ReadsInventoryWithoutOpeningCamera()
    {
        await WithResponse("cameras.list", new { cameras = new[] { new { id = 7, name = "ASI676MC", is_color = true } }, scanned_at_unix_ms = 1, error = (string?)null }, async client =>
        {
            var inventory = await client.GetCamerasAsync();
            Assert.Equal(7, Assert.Single(inventory.Cameras).Id);
        });
    }

    [Theory]
    [InlineData("{\"cameras\":[],\"error\":null}")]
    [InlineData("{\"cameras\":null,\"scanned_at_unix_ms\":null,\"error\":null}")]
    [InlineData("{\"cameras\":[{\"id\":-1,\"name\":\"ASI\",\"is_color\":true}],\"scanned_at_unix_ms\":1,\"error\":null}")]
    [InlineData("{\"cameras\":[{\"id\":1,\"name\":\"A\",\"is_color\":true},{\"id\":1,\"name\":\"B\",\"is_color\":true}],\"scanned_at_unix_ms\":1,\"error\":null}")]
    public async Task RejectsMalformedInventories(string json)
    {
        using var result = JsonDocument.Parse(json);
        await WithResponse("cameras.list", result.RootElement, async client =>
            await Assert.ThrowsAsync<AgentProtocolException>(() => client.GetCamerasAsync()));
    }

    [Theory]
    [InlineData(true, "capture.pause")]
    [InlineData(false, "capture.resume")]
    public async Task PauseAndResumeUseExistingControlProtocol(bool paused, string method)
    {
        await WithResponse(method, new { accepted = true }, client => client.SetPausedAsync(paused));
    }
}
