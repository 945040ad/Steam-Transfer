using System.Buffers;
using System.Diagnostics;

namespace SteamTransfer;

public sealed class FileEntry
{
    public string Path { get; set; } = "";   // relative to library root, forward slashes, e.g. steamapps/common/Game/x.pak
    public long Size { get; set; }
    public long MTime { get; set; }          // UTC ticks
}

/// <summary>Describes one game package (sent over the network or saved as transfer.json on a drive).</summary>
public sealed class Offer
{
    public int Version { get; set; } = 1;
    public string AppId { get; set; } = "";
    public string Name { get; set; } = "";
    public string From { get; set; } = "";
    public long TotalBytes { get; set; }
    public List<string> Dirs { get; set; } = new();
    public List<FileEntry> Files { get; set; } = new();
}

public sealed class Reply
{
    public bool Accept { get; set; }
    public string? Reason { get; set; }
    public List<int> Skip { get; set; } = new();   // file indices the receiver already has
    public string? Session { get; set; }           // data connections join with this id
    public int Streams { get; set; } = 1;          // number of parallel data connections to open
}

public sealed class Result
{
    public bool Ok { get; set; }
    public string? Error { get; set; }
}

/// <summary>Thread-safe progress + pause gate shared between the transfer tasks and the UI.</summary>
public sealed class TransferProgress
{
    long _done;
    public long Total;
    public volatile string Current = "";
    public volatile string? Waiting;   // set while waiting for a disconnected drive
    public int DriveWaiters;           // parallel streams waiting on the same drive (log once)
    public readonly Stopwatch Clock = Stopwatch.StartNew();
    readonly object _gate = new();
    TaskCompletionSource? _resume;

    public long Done => Interlocked.Read(ref _done);
    public void Add(long n) => Interlocked.Add(ref _done, n);

    public bool Paused { get { lock (_gate) return _resume != null; } }

    public void Pause()
    {
        lock (_gate)
        {
            if (_resume != null) return;
            _resume = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            Clock.Stop();
        }
    }

    public void Resume()
    {
        TaskCompletionSource? t;
        lock (_gate) { t = _resume; _resume = null; Clock.Start(); }
        t?.TrySetResult();
    }

    public Task WaitIfPausedAsync(CancellationToken ct)
    {
        Task? t;
        lock (_gate) t = _resume?.Task;
        return t == null ? Task.CompletedTask : t.WaitAsync(ct);
    }
}

public static class Transfer
{
    const int BufSize = 4 * 1024 * 1024;
    static readonly TimeSpan MTimeTolerance = TimeSpan.FromSeconds(2); // FAT/exFAT drives round timestamps

    /// <summary>Build the list of everything that makes up an installed game, manifests last.</summary>
    public static Offer BuildOffer(SteamGame g)
    {
        var offer = new Offer { AppId = g.AppId, Name = g.Name, From = Environment.MachineName };
        var root = g.Library.Path;
        var opts = new EnumerationOptions { RecurseSubdirectories = true, AttributesToSkip = 0, IgnoreInaccessible = false };

        void AddTree(string rel)
        {
            var full = System.IO.Path.Combine(root, rel.Replace('/', '\\'));
            if (!Directory.Exists(full)) return;
            offer.Dirs.Add(rel);
            foreach (var d in Directory.EnumerateDirectories(full, "*", opts))
                offer.Dirs.Add(Rel(root, d));
            foreach (var f in Directory.EnumerateFiles(full, "*", opts))
                AddFile(f);
        }
        void AddFile(string full)
        {
            var fi = new FileInfo(full);
            if (!fi.Exists) return;
            offer.Files.Add(new FileEntry { Path = Rel(root, full), Size = fi.Length, MTime = fi.LastWriteTimeUtc.Ticks });
        }

        AddTree($"steamapps/common/{g.InstallDir}");
        AddTree($"steamapps/workshop/content/{g.AppId}");
        // Manifests go last so Steam never sees a half-copied game as installed.
        AddFile(System.IO.Path.Combine(root, "steamapps", "workshop", $"appworkshop_{g.AppId}.acf"));
        AddFile(System.IO.Path.Combine(root, "steamapps", $"appmanifest_{g.AppId}.acf"));

        if (!offer.Files.Any(f => f.Path.EndsWith($"appmanifest_{g.AppId}.acf", StringComparison.OrdinalIgnoreCase)))
            throw new InvalidOperationException("Game manifest not found.");
        offer.TotalBytes = offer.Files.Sum(f => f.Size);
        return offer;
    }

    static string Rel(string root, string full) => System.IO.Path.GetRelativePath(root, full).Replace('\\', '/');

    /// <summary>Resolve a relative path from a peer, refusing anything outside the allowed Steam folders.</summary>
    public static string SafeCombine(string root, string rel)
    {
        var r = rel.Replace('\\', '/');
        if (r.Split('/').Any(seg => seg is ".." or "." || seg.Contains(':')))
            throw new InvalidDataException($"Refusing unsafe path: {rel}");
        bool allowed = r.StartsWith("steamapps/common/", StringComparison.OrdinalIgnoreCase)
                    || r.StartsWith("steamapps/workshop/", StringComparison.OrdinalIgnoreCase)
                    || (r.StartsWith("steamapps/appmanifest_", StringComparison.OrdinalIgnoreCase) && !r[10..].Contains('/'));
        if (!allowed) throw new InvalidDataException($"Refusing unexpected path: {rel}");
        var rootFull = System.IO.Path.GetFullPath(root).TrimEnd('\\') + "\\";
        var full = System.IO.Path.GetFullPath(System.IO.Path.Combine(rootFull, r.Replace('/', '\\')));
        if (!full.StartsWith(rootFull, StringComparison.OrdinalIgnoreCase)) throw new InvalidDataException($"Refusing path outside library: {rel}");
        return full;
    }

    /// <summary>Indices of files that already exist at the destination with identical size and timestamp (resume / update).</summary>
    public static List<int> ComputeSkip(Offer offer, string destRoot)
    {
        var skip = new List<int>();
        for (int i = 0; i < offer.Files.Count; i++)
        {
            var f = offer.Files[i];
            if (IsManifest(f.Path)) continue; // always rewrite manifests
            try
            {
                var fi = new FileInfo(SafeCombine(destRoot, f.Path));
                if (fi.Exists && fi.Length == f.Size && (fi.LastWriteTimeUtc - new DateTime(f.MTime, DateTimeKind.Utc)).Duration() <= MTimeTolerance)
                    skip.Add(i);
            }
            catch { }
        }
        return skip;
    }

    public static bool IsManifest(string rel) => rel.EndsWith(".acf", StringComparison.OrdinalIgnoreCase);

    public static void CreateDirs(Offer offer, string destRoot)
    {
        foreach (var d in offer.Dirs)
            Directory.CreateDirectory(SafeCombine(destRoot, d));
    }

    public static FileStream OpenRead(string path) =>
        new(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete, 0, FileOptions.SequentialScan | FileOptions.Asynchronous);

    /// <summary>Receive/copy exactly f.Size bytes from src into the destination file, then stamp its timestamp.
    /// flush: force the data onto the device before the file counts as done (for removable drives that may be yanked).</summary>
    public static async Task WriteFileAsync(Stream src, FileEntry f, string destRoot, TransferProgress p, CancellationToken ct,
        bool flush = false, Func<Exception, long, Task<Stream?>>? onReadError = null)
    {
        var dest = SafeCombine(destRoot, f.Path);
        Directory.CreateDirectory(System.IO.Path.GetDirectoryName(dest)!);
        var tmp = IsManifest(f.Path) ? dest + ".steamtransfer.tmp" : dest;
        p.Current = f.Path;

        if (File.Exists(tmp)) File.SetAttributes(tmp, FileAttributes.Normal);
        await using (var fs = new FileStream(tmp, FileMode.Create, FileAccess.Write, FileShare.None, 0, FileOptions.SequentialScan | FileOptions.Asynchronous))
        {
            if (f.Size > 0) fs.SetLength(f.Size); // preallocate, less fragmentation
            await PumpAsync(src, fs, f.Size, p, ct, onReadError);
            if (flush) fs.Flush(flushToDisk: true);
        }
        if (tmp != dest) File.Move(tmp, dest, overwrite: true);
        File.SetLastWriteTimeUtc(dest, new DateTime(f.MTime, DateTimeKind.Utc));
        if (System.IO.Path.GetFileName(dest).StartsWith("appmanifest_", StringComparison.OrdinalIgnoreCase))
            Steam.FixManifest(dest);
    }

    /// <summary>Double-buffered copy: reads the next chunk while the previous one is being written.
    /// Honors pause. onReadError(error, bytesReadSoFar) may return a replacement source positioned at that offset
    /// (e.g. after a disconnected drive comes back), or null to give up.</summary>
    public static async Task PumpAsync(Stream src, Stream dst, long count, TransferProgress p, CancellationToken ct,
        Func<Exception, long, Task<Stream?>>? onReadError = null)
    {
        var a = ArrayPool<byte>.Shared.Rent(BufSize);
        var b = ArrayPool<byte>.Shared.Rent(BufSize);
        try
        {
            Task? pending = null;
            int pendingLen = 0;
            long read = 0;
            while (read < count)
            {
                await p.WaitIfPausedAsync(ct);
                int n;
                while (true)
                {
                    try
                    {
                        n = await src.ReadAsync(a.AsMemory(0, (int)Math.Min(BufSize, count - read)), ct);
                        if (n == 0) throw new EndOfStreamException("Source ended early (connection lost or file changed).");
                        break;
                    }
                    catch (Exception ex) when (onReadError != null && !ct.IsCancellationRequested && ex is IOException or UnauthorizedAccessException)
                    {
                        var replacement = await onReadError(ex, read);
                        if (replacement == null) throw;
                        src = replacement;
                    }
                }
                if (pending != null) { await pending; p.Add(pendingLen); }
                pending = dst.WriteAsync(a, 0, n, ct);
                pendingLen = n;
                read += n;
                (a, b) = (b, a);
            }
            if (pending != null) { await pending; p.Add(pendingLen); }
        }
        finally
        {
            ArrayPool<byte>.Shared.Return(a);
            ArrayPool<byte>.Shared.Return(b);
        }
    }

    // ---------------- disconnected drives ----------------

    public static string DriveOf(string path)
    {
        try { return System.IO.Path.GetPathRoot(System.IO.Path.GetFullPath(path)) ?? path; } catch { return path; }
    }

    public static bool IsDriveGone(string path)
    {
        try { return !Directory.Exists(DriveOf(path)); } catch { return true; }
    }

    /// <summary>After an I/O error: give Windows a moment to finish removing the device, then report which drive vanished (or null).</summary>
    public static async Task<string?> FindDisconnectedDriveAsync(params string[] paths)
    {
        for (int i = 0; i < 6; i++)
        {
            foreach (var path in paths) if (IsDriveGone(path)) return DriveOf(path);
            await Task.Delay(500);
        }
        return null;
    }

    /// <summary>Wait (as a paused transfer) until the drive comes back.</summary>
    public static async Task WaitForDriveAsync(string path, TransferProgress p, Action<string> log, CancellationToken ct)
    {
        var drive = DriveOf(path).TrimEnd('\\');
        var msg = $"Drive {drive} disconnected — plug it back in to continue.";
        p.Waiting = msg;
        if (Interlocked.Increment(ref p.DriveWaiters) == 1) log(msg);
        bool back = false;
        try
        {
            while (IsDriveGone(path)) await Task.Delay(1000, ct);
            await Task.Delay(2000, ct); // let Windows finish mounting it
            back = true;
        }
        finally
        {
            if (Interlocked.Decrement(ref p.DriveWaiters) == 0)
            {
                p.Waiting = null;
                if (back) log($"Drive {drive} is back — continuing.");
            }
        }
    }

    // ---------------- Drive (USB) export / import ----------------

    public const string PackageFile = "transfer.json";

    /// <summary>Copy a game to a folder on an external drive (resumable).</summary>
    public static async Task<string> ExportAsync(SteamGame g, string targetFolder, TransferProgress p, Action<string> log, CancellationToken ct)
    {
        var offer = BuildOffer(g);
        var safeName = string.Concat(g.Name.Select(c => System.IO.Path.GetInvalidFileNameChars().Contains(c) ? '_' : c)).Trim();
        var pkg = System.IO.Path.Combine(targetFolder, "SteamTransfer", $"{g.AppId} - {safeName}");
        Directory.CreateDirectory(pkg);
        var marker = System.IO.Path.Combine(pkg, PackageFile);
        if (File.Exists(marker)) File.Delete(marker); // incomplete until rewritten at the end

        var needed = offer.TotalBytes - ComputeSkip(offer, pkg).Sum(i => offer.Files[i].Size);
        var free = new DriveInfo(System.IO.Path.GetPathRoot(pkg)!).AvailableFreeSpace;
        if (free < needed) throw new IOException($"Not enough space on target drive: need {Util.Size(needed)}, have {Util.Size(free)}.");

        await CopyLocalAsync(offer, g.Library.Path, pkg, p, log, ct, flush: true);
        await File.WriteAllTextAsync(marker, System.Text.Json.JsonSerializer.Serialize(offer), ct);
        return pkg;
    }

    /// <summary>Install a package folder (from a drive) into a local library.</summary>
    public static Task ImportAsync(Offer offer, string packageFolder, SteamLibrary lib, TransferProgress p, Action<string> log, CancellationToken ct) =>
        CopyLocalAsync(offer, packageFolder, lib.Path, p, log, ct, flush: false);

    /// <summary>File-by-file copy between drives. If either drive is unplugged mid-copy, waits for it to come back
    /// and redoes the interrupted file from the start (anything half-written to a yanked drive can't be trusted).</summary>
    static async Task CopyLocalAsync(Offer offer, string srcRoot, string destRoot, TransferProgress p, Action<string> log, CancellationToken ct, bool flush)
    {
        var skip = ComputeSkip(offer, destRoot).ToHashSet();
        p.Total = offer.Files.Where((f, i) => !skip.Contains(i)).Sum(f => f.Size);
        if (skip.Count > 0) log($"{skip.Count} files already present, skipping them.");
        await RetryOnDisconnect(() => { CreateDirs(offer, destRoot); return Task.CompletedTask; });

        for (int i = 0; i < offer.Files.Count; i++)
        {
            if (skip.Contains(i)) continue;
            var f = offer.Files[i];
            long before = p.Done;
            await RetryOnDisconnect(async () =>
            {
                p.Add(before - p.Done); // undo progress of a failed attempt
                await using var src = OpenRead(SafeCombine(srcRoot, f.Path));
                if (src.Length != f.Size) throw new IOException($"File changed size since the copy started: {f.Path}");
                await WriteFileAsync(src, f, destRoot, p, ct, flush);
            });
        }

        async Task RetryOnDisconnect(Func<Task> step)
        {
            while (true)
            {
                ct.ThrowIfCancellationRequested();
                try { await step(); return; }
                catch (Exception ex) when (!ct.IsCancellationRequested && ex is IOException or UnauthorizedAccessException)
                {
                    var gone = await FindDisconnectedDriveAsync(srcRoot, destRoot);
                    if (gone == null) throw;
                    await WaitForDriveAsync(gone, p, log, ct);
                }
            }
        }
    }
}

public static class Util
{
    public static string Size(long b)
    {
        string[] u = { "B", "KB", "MB", "GB", "TB" };
        double v = b; int i = 0;
        while (v >= 1024 && i < u.Length - 1) { v /= 1024; i++; }
        return $"{v:0.##} {u[i]}";
    }

    public static string Time(TimeSpan t) =>
        t.TotalHours >= 1 ? $"{(int)t.TotalHours}h {t.Minutes:00}m" : t.TotalMinutes >= 1 ? $"{t.Minutes}m {t.Seconds:00}s" : $"{t.Seconds}s";
}
