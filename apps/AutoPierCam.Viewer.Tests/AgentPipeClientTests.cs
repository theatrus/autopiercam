using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text.Json;
using System.Text.Json.Nodes;
using AutoPierCam.Viewer;
using Xunit;

public sealed class AgentPipeClientTests
{
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
