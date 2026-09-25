using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text.Json;
using AutoPierCam.Viewer;
using Xunit;

public sealed class AgentPipeClientTests
{
    private static async Task WithResponse(string method, object result, Func<AgentPipeClient, Task> assertion)
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
