using System.Collections.Concurrent;
using System.Diagnostics;
using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;
using System.Threading.Channels;

namespace SteamTransfer;

public sealed record Peer(string Name, IPAddress Address, string Via, long LinkBps, string Kind, bool Manual)
{
    public bool Thunderbolt => Kind == "usb4";
    public string Key => Address.ToString();
}

/// <summary>Finds other PCs running this app by UDP broadcast on every network adapter.
/// Works on a direct ethernet cable and on Thunderbolt/USB4 networking: Windows gives
/// both ends a 169.254.x.x address automatically.</summary>
public sealed class Discovery : IDisposable
{
    public const int Port = 47800;
    const string Magic = "STEAMXFER2";
    readonly string _id = Guid.NewGuid().ToString("N");
    readonly ConcurrentDictionary<string, (Peer Peer, DateTime Seen)> _peers = new();
    readonly CancellationTokenSource _cts = new();
    UdpClient? _listener;

    public void Start()
    {
        _listener = new UdpClient(AddressFamily.InterNetwork);
        _listener.Client.SetSocketOption(SocketOptionLevel.Socket, SocketOptionName.ReuseAddress, true);
        _listener.Client.Bind(new IPEndPoint(IPAddress.Any, Port));
        _listener.EnableBroadcast = true;
        _ = Task.Run(ListenLoop);
        _ = Task.Run(BeaconLoop);
    }

    /// <summary>Thunderbolt/USB4 links first, then fastest link.</summary>
    public List<Peer> Peers
    {
        get
        {
            var cutoff = DateTime.UtcNow.AddSeconds(-8);
            foreach (var kv in _peers.Where(kv => kv.Value.Seen < cutoff).ToList()) _peers.TryRemove(kv.Key, out _);
            return _peers.Values.Select(v => v.Peer)
                .OrderByDescending(p => p.Thunderbolt).ThenByDescending(p => p.LinkBps).ThenBy(p => p.Name).ToList();
        }
    }

    async Task ListenLoop()
    {
        while (!_cts.IsCancellationRequested)
        {
            try
            {
                var r = await _listener!.ReceiveAsync(_cts.Token);
                var parts = Encoding.UTF8.GetString(r.Buffer).Split('|');
                if (parts.Length < 3 || parts[0] != Magic || parts[2] == _id) continue;
                var ip = r.RemoteEndPoint.Address;
                var route = LocalRouteTo(ip);
                _peers[ip.ToString()] = (new Peer(parts[1], ip, route.Via, route.Bps, route.Kind, false), DateTime.UtcNow);
            }
            catch (OperationCanceledException) { return; }
            catch { await Task.Delay(200); }
        }
    }

    async Task BeaconLoop()
    {
        var msg = Encoding.UTF8.GetBytes($"{Magic}|{Environment.MachineName}|{_id}");
        while (!_cts.IsCancellationRequested)
        {
            foreach (var (local, bcast, _, _) in LocalIPv4())
            {
                try
                {
                    using var u = new UdpClient(new IPEndPoint(local, 0)) { EnableBroadcast = true };
                    await u.SendAsync(msg, msg.Length, new IPEndPoint(bcast, Port));
                    await u.SendAsync(msg, msg.Length, new IPEndPoint(IPAddress.Broadcast, Port));
                }
                catch { }
            }
            try { await Task.Delay(2000, _cts.Token); } catch { return; }
        }
    }

    /// <summary>(local address, subnet broadcast, adapter, mask) for every active IPv4 adapter.</summary>
    public static IEnumerable<(IPAddress Local, IPAddress Broadcast, NetworkInterface Nic, IPAddress Mask)> LocalIPv4()
    {
        foreach (var nic in NetworkInterface.GetAllNetworkInterfaces())
        {
            if (nic.OperationalStatus != OperationalStatus.Up || nic.NetworkInterfaceType == NetworkInterfaceType.Loopback) continue;
            foreach (var ua in nic.GetIPProperties().UnicastAddresses)
            {
                if (ua.Address.AddressFamily != AddressFamily.InterNetwork || ua.IPv4Mask == null) continue;
                var ip = ua.Address.GetAddressBytes();
                var mask = ua.IPv4Mask.GetAddressBytes();
                var b = new byte[4];
                for (int i = 0; i < 4; i++) b[i] = (byte)(ip[i] | ~mask[i]);
                yield return (ua.Address, new IPAddress(b), nic, ua.IPv4Mask);
            }
        }
    }

    public static bool IsThunderbolt(NetworkInterface nic) =>
        nic.Description.Contains("Thunderbolt", StringComparison.OrdinalIgnoreCase)
        || nic.Description.Contains("USB4", StringComparison.OrdinalIgnoreCase)
        || nic.Name.Contains("Thunderbolt", StringComparison.OrdinalIgnoreCase)
        || nic.Name.Contains("USB4", StringComparison.OrdinalIgnoreCase);

    public static string KindOf(NetworkInterface nic) =>
        IsThunderbolt(nic) ? "usb4" : nic.NetworkInterfaceType == NetworkInterfaceType.Wireless80211 ? "wifi" : "ethernet";

    /// <summary>Which local adapter reaches this address, its speed, and the link kind (usb4 / ethernet / wifi).</summary>
    public static (string Via, long Bps, bool Thunderbolt, string Kind) LocalRouteTo(IPAddress remote)
    {
        if (IPAddress.IsLoopback(remote)) return ("this PC", 100_000_000_000, false, "ethernet");
        var r = remote.GetAddressBytes();
        foreach (var (local, _, nic, mask) in LocalIPv4())
        {
            var l = local.GetAddressBytes(); var m = mask.GetAddressBytes();
            bool same = true;
            for (int i = 0; i < 4; i++) if ((l[i] & m[i]) != (r[i] & m[i])) same = false;
            if (!same) continue;
            var kind = KindOf(nic);
            return (nic.Name, nic.Speed, kind == "usb4", kind);
        }
        return ("routed", 0, false, "ethernet");
    }

    public void Dispose()
    {
        _cts.Cancel();
        _listener?.Dispose();
    }
}

/// <summary>Adds inbound firewall rules once (UAC prompt), for all network profiles â€”
/// direct cable / Thunderbolt links show up as "Unidentified/Public" networks, which Windows blocks by default.</summary>
public static class Firewall
{
    const string RuleName = "Steam Game Transfer";

    public static bool RuleExists()
    {
        try
        {
            var psi = new ProcessStartInfo("netsh", $"advfirewall firewall show rule name=\"{RuleName}\"")
            { UseShellExecute = false, CreateNoWindow = true, RedirectStandardOutput = true };
            using var p = Process.Start(psi)!;
            p.StandardOutput.ReadToEnd();
            p.WaitForExit();
            return p.ExitCode == 0;
        }
        catch { return false; }
    }

    public static bool AddRules()
    {
        var args = $"/c netsh advfirewall firewall add rule name=\"{RuleName}\" dir=in action=allow protocol=TCP localport={Server.Port} profile=any"
                 + $" & netsh advfirewall firewall add rule name=\"{RuleName}\" dir=in action=allow protocol=UDP localport={Discovery.Port} profile=any";
        try
        {
            using var p = Process.Start(new ProcessStartInfo("cmd.exe", args) { UseShellExecute = true, Verb = "runas", WindowStyle = ProcessWindowStyle.Hidden })!;
            p.WaitForExit();
            return RuleExists();
        }
        catch { return false; } // user declined UAC
    }

    public static void RemoveRules()
    {
        try
        {
            using var p = Process.Start(new ProcessStartInfo("netsh", $"advfirewall firewall delete rule name=\"{RuleName}\"")
            { UseShellExecute = true, Verb = "runas", WindowStyle = ProcessWindowStyle.Hidden })!;
            p.WaitForExit();
        }
        catch { }
    }
}

/// <summary>First message on every connection: either a new offer (control connection) or joining an accepted session (data connection).</summary>
public sealed class Hello
{
    public Offer? Offer { get; set; }
    public int Streams { get; set; }        // data connections the sender would like to use
    public string? Session { get; set; }    // set on data connections
}

/// <summary>Length-prefixed JSON control messages. File data is sent as [int32 file index][raw bytes], index -1 = end of stream.</summary>
public static class Proto
{
    public const int EndMarker = -1;
    public const int MaxStreams = 8;

    public static async Task SendAsync<T>(Stream s, T msg, CancellationToken ct)
    {
        var body = JsonSerializer.SerializeToUtf8Bytes(msg);
        await s.WriteAsync(BitConverter.GetBytes(body.Length), ct);
        await s.WriteAsync(body, ct);
        await s.FlushAsync(ct);
    }

    public static async Task<T> ReadAsync<T>(Stream s, CancellationToken ct)
    {
        var len = new byte[4];
        await s.ReadExactlyAsync(len, ct);
        int n = BitConverter.ToInt32(len);
        if (n <= 0 || n > 512 * 1024 * 1024) throw new InvalidDataException("Bad message from peer.");
        var body = new byte[n];
        await s.ReadExactlyAsync(body, ct);
        return JsonSerializer.Deserialize<T>(body) ?? throw new InvalidDataException("Empty message from peer.");
    }

    public static void Tune(Socket s)
    {
        s.NoDelay = true;
        s.SendBufferSize = 8 * 1024 * 1024;
        s.ReceiveBufferSize = 8 * 1024 * 1024;
    }

    /// <summary>More parallel streams on fast links (Thunderbolt/USB4, 2.5G+ ethernet) to keep the pipe and the disks busy.</summary>
    public static int StreamsFor(long linkBps, bool thunderbolt) =>
        thunderbolt || linkBps >= 5_000_000_000 ? 4 : linkBps >= 2_000_000_000 ? 3 : 2;
}

public sealed class ReceiveDecision
{
    public required SteamLibrary Library { get; init; }
    public bool CloseSteam { get; init; }
}

public interface ITransferHost
{
    void Log(string msg);
    /// <summary>Returns null if another transfer is running.</summary>
    (TransferProgress Progress, CancellationToken Token)? BeginJob(string title);
    void EndJob(Exception? error, string? message);
    Task<ReceiveDecision?> AskAcceptAsync(Offer offer, string fromIp);
    Task AfterReceiveAsync(Offer offer, bool steamWasClosed);
}

public static class Sender
{
    public static async Task SendAsync(SteamGame game, IPAddress to, TransferProgress p, Action<string> log, CancellationToken outerCt)
    {
        log($"Scanning files of {game.Name}...");
        p.Current = "scanning files";
        var offer = await Task.Run(() => Transfer.BuildOffer(game), outerCt);
        var route = Discovery.LocalRouteTo(to);
        var want = Proto.StreamsFor(route.Bps, route.Thunderbolt);
        log($"{offer.Files.Count:N0} files, {Util.Size(offer.TotalBytes)}. Connecting to {to} via {route.Via}...");

        using var cts = CancellationTokenSource.CreateLinkedTokenSource(outerCt);
        var ct = cts.Token;
        var clients = new ConcurrentBag<TcpClient>();
        using var reg = ct.Register(() => { foreach (var c in clients) c.Close(); });
        try
        {
            var control = await ConnectAsync(to, ct);
            clients.Add(control);
            var ns = control.GetStream();

            await Proto.SendAsync(ns, new Hello { Offer = offer, Streams = want }, ct);
            p.Current = "waiting for the other PC to accept";
            log("Waiting for the other PC to accept...");
            var reply = await Proto.ReadAsync<Reply>(ns, ct);
            if (!reply.Accept) throw new IOException($"Transfer declined: {reply.Reason ?? "rejected by other PC"}");

            var skip = reply.Skip.ToHashSet();
            var todo = Enumerable.Range(0, offer.Files.Count).Where(i => !skip.Contains(i)).ToList();
            p.Total = todo.Sum(i => offer.Files[i].Size);
            if (skip.Count > 0) log($"Other PC already has {skip.Count:N0} files, skipping them.");

            // Game files: biggest first, spread over parallel connections. Manifests: last, on the control connection.
            var queue = new ConcurrentQueue<int>(todo.Where(i => !Transfer.IsManifest(offer.Files[i].Path)).OrderByDescending(i => offer.Files[i].Size));
            int streams = Math.Clamp(reply.Streams, 1, Proto.MaxStreams);
            log($"Sending over {streams} parallel stream{(streams > 1 ? "s" : "")}.");

            var workers = new List<Task>();
            for (int k = 0; k < streams; k++)
            {
                var dc = await ConnectAsync(to, ct);
                clients.Add(dc);
                await Proto.SendAsync(dc.GetStream(), new Hello { Session = reply.Session }, ct);
                workers.Add(Task.Run(async () =>
                {
                    try
                    {
                        var ds = dc.GetStream();
                        while (queue.TryDequeue(out var idx)) await SendFileAsync(ds, game, offer, idx, p, log, ct);
                        await ds.WriteAsync(BitConverter.GetBytes(Proto.EndMarker), ct);
                        await ds.FlushAsync(ct);
                    }
                    catch { cts.Cancel(); throw; }
                }, ct));
            }
            await Task.WhenAll(workers);

            foreach (var idx in todo.Where(i => Transfer.IsManifest(offer.Files[i].Path)))
                await SendFileAsync(ns, game, offer, idx, p, log, ct);
            await ns.WriteAsync(BitConverter.GetBytes(Proto.EndMarker), ct);
            await ns.FlushAsync(ct);

            p.Current = "finishing up on other PC";
            var result = await Proto.ReadAsync<Result>(ns, ct);
            if (!result.Ok) throw new IOException($"Other PC reported an error: {result.Error}");
        }
        finally
        {
            foreach (var c in clients) c.Dispose();
        }
    }

    static async Task SendFileAsync(Stream s, SteamGame game, Offer offer, int idx, TransferProgress p, Action<string> log, CancellationToken ct)
    {
        var f = offer.Files[idx];
        var path = Transfer.SafeCombine(game.Library.Path, f.Path);
        p.Current = f.Path;
        FileStream? fs = null;
        while (fs == null) // the game's drive may be unplugged right now
        {
            try { fs = Transfer.OpenRead(path); }
            catch (Exception) when (Transfer.IsDriveGone(path)) { await Transfer.WaitForDriveAsync(path, p, log, ct); }
        }
        try
        {
            await s.WriteAsync(BitConverter.GetBytes(idx), ct);
            // If the game's drive disconnects mid-file, wait for it and continue from the same byte;
            // the other PC simply waits on the open connection.
            await Transfer.PumpAsync(fs, s, f.Size, p, ct, async (_, offset) =>
            {
                if (await Transfer.FindDisconnectedDriveAsync(path) == null) return null;
                try { fs.Dispose(); } catch { }
                await Transfer.WaitForDriveAsync(path, p, log, ct);
                fs = Transfer.OpenRead(path);
                if (fs.Length != f.Size) throw new IOException($"{f.Path} changed while its drive was disconnected.");
                fs.Seek(offset, SeekOrigin.Begin);
                return fs;
            });
        }
        finally { try { fs.Dispose(); } catch { } }
    }

    static async Task<TcpClient> ConnectAsync(IPAddress to, CancellationToken ct)
    {
        var client = new TcpClient(to.AddressFamily);
        Proto.Tune(client.Client);
        using var cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        cts.CancelAfter(TimeSpan.FromSeconds(8));
        try { await client.ConnectAsync(to, Server.Port, cts.Token); return client; }
        catch (OperationCanceledException) when (!ct.IsCancellationRequested)
        {
            client.Dispose();
            throw new IOException($"Could not reach {to}. Is the app open on the other PC?");
        }
        catch { client.Dispose(); throw; }
    }
}

/// <summary>Always-on listener that accepts games from other PCs.</summary>
public sealed class Server : IDisposable
{
    public static int Port = 47801; // settable only so tests can run next to a live copy of the app
    readonly ITransferHost _host;
    readonly TcpListener _listener = new(IPAddress.Any, Port);
    // (Port is read when the Server is constructed)
    readonly CancellationTokenSource _cts = new();
    readonly ConcurrentDictionary<string, Channel<TcpClient>> _sessions = new();

    public Server(ITransferHost host) => _host = host;

    public void Start()
    {
        _listener.Start();
        _ = Task.Run(AcceptLoop);
    }

    async Task AcceptLoop()
    {
        while (!_cts.IsCancellationRequested)
        {
            try
            {
                var c = await _listener.AcceptTcpClientAsync(_cts.Token);
                _ = Task.Run(() => Handle(c));
            }
            catch (OperationCanceledException) { return; }
            catch { await Task.Delay(200); }
        }
    }

    async Task Handle(TcpClient client)
    {
        Hello hello;
        try
        {
            Proto.Tune(client.Client);
            using var t = new CancellationTokenSource(TimeSpan.FromSeconds(30));
            hello = await Proto.ReadAsync<Hello>(client.GetStream(), t.Token);
        }
        catch { client.Dispose(); return; }

        if (hello.Session != null)
        {
            // Data connection for an accepted transfer: hand it over to that session.
            if (!_sessions.TryGetValue(hello.Session, out var ch) || !ch.Writer.TryWrite(client)) client.Dispose();
            return;
        }
        if (hello.Offer == null) { client.Dispose(); return; }
        using (client) await HandleOffer(client, hello.Offer, hello.Streams);
    }

    async Task HandleOffer(TcpClient client, Offer offer, int wantStreams)
    {
        var from = ((IPEndPoint)client.Client.RemoteEndPoint!).Address;
        var ns = client.GetStream();

        var job = _host.BeginJob($"Receiving {offer.Name}");
        if (job == null)
        {
            await Proto.SendAsync(ns, new Reply { Accept = false, Reason = "the other PC is busy with another transfer" }, CancellationToken.None);
            return;
        }
        var (p, jobCt) = job.Value;
        using var cts = CancellationTokenSource.CreateLinkedTokenSource(jobCt, _cts.Token);
        var ct = cts.Token;
        var dataClients = new ConcurrentBag<TcpClient>();
        using var reg = ct.Register(() => { client.Close(); foreach (var c in dataClients) c.Close(); });
        var session = Guid.NewGuid().ToString("N");
        bool closedSteam = false;
        string? root = null;
        try
        {
            _host.Log($"{offer.From} ({from}) wants to send {offer.Name} ({Util.Size(offer.TotalBytes)}).");
            var decision = await _host.AskAcceptAsync(offer, from.ToString());
            if (decision == null)
            {
                await Proto.SendAsync(ns, new Reply { Accept = false, Reason = "declined by user" }, ct);
                _host.EndJob(null, "Declined.");
                return;
            }
            root = decision.Library.Path;
            var skip = await Task.Run(() => Transfer.ComputeSkip(offer, root), ct);
            if (decision.CloseSteam && Steam.IsRunning) { await Steam.ShutdownAsync(_host.Log); closedSteam = true; }
            Transfer.CreateDirs(offer, root);

            var skipSet = skip.ToHashSet();
            var expected = Enumerable.Range(0, offer.Files.Count).Where(i => !skipSet.Contains(i)).ToHashSet();
            var received = new HashSet<int>();
            p.Total = expected.Sum(i => offer.Files[i].Size);

            int streams = Math.Clamp(wantStreams, 1, Proto.MaxStreams);
            var channel = Channel.CreateUnbounded<TcpClient>();
            _sessions[session] = channel;
            await Proto.SendAsync(ns, new Reply { Accept = true, Skip = skip, Session = session, Streams = streams }, ct);
            _host.Log($"Receiving into {root} over {streams} stream{(streams > 1 ? "s" : "")}"
                      + (skip.Count > 0 ? $" ({skip.Count:N0} files already here)" : ""));

            async Task ReceiveLoop(Stream s, bool manifests)
            {
                var hdr = new byte[4];
                while (true)
                {
                    await s.ReadExactlyAsync(hdr, ct);
                    int idx = BitConverter.ToInt32(hdr);
                    if (idx == Proto.EndMarker) return;
                    bool ok;
                    lock (received) ok = expected.Contains(idx) && received.Add(idx);
                    if (!ok || Transfer.IsManifest(offer.Files[idx].Path) != manifests)
                        throw new InvalidDataException("Peer sent an unexpected file.");
                    await Transfer.WriteFileAsync(s, offer.Files[idx], root, p, ct);
                }
            }

            var workers = Enumerable.Range(0, streams).Select(_ => Task.Run(async () =>
            {
                try
                {
                    using var joinTimeout = CancellationTokenSource.CreateLinkedTokenSource(ct);
                    joinTimeout.CancelAfter(TimeSpan.FromSeconds(30));
                    var dc = await channel.Reader.ReadAsync(joinTimeout.Token);
                    dataClients.Add(dc);
                    if (ct.IsCancellationRequested) dc.Close();
                    using (dc) await ReceiveLoop(dc.GetStream(), manifests: false);
                }
                catch { cts.Cancel(); throw; }
            }, ct)).ToList();
            await Task.WhenAll(workers);
            await ReceiveLoop(ns, manifests: true);

            if (received.Count != expected.Count) throw new IOException($"Transfer incomplete: {expected.Count - received.Count} files missing.");
            await Proto.SendAsync(ns, new Result { Ok = true }, ct);
            _host.EndJob(null, $"Received {offer.Name}.");
            await _host.AfterReceiveAsync(offer, closedSteam);
        }
        catch (Exception ex)
        {
            if (root != null && ex is IOException or UnauthorizedAccessException && await Transfer.FindDisconnectedDriveAsync(root) is string drive)
                ex = new IOException($"Drive {drive.TrimEnd('\\')} was disconnected. Plug it back in and send again — files already copied are kept and skipped.", ex);
            try { await Proto.SendAsync(ns, new Result { Ok = false, Error = ex.Message }, CancellationToken.None); } catch { }
            _host.EndJob(ex, null);
            if (closedSteam) await Steam.RestartAsync(_host.Log);
        }
        finally
        {
            if (_sessions.TryRemove(session, out var ch))
                while (ch.Reader.TryRead(out var stray)) stray.Dispose();
            foreach (var c in dataClients) c.Dispose();
        }
    }

    public void Dispose()
    {
        _cts.Cancel();
        _listener.Stop();
    }
}
