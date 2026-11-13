using System.Diagnostics;
using System.Text;
using System.Text.RegularExpressions;
using Microsoft.Win32;

namespace SteamTransfer;

public sealed record SteamLibrary(string Path)
{
    public string SteamApps => System.IO.Path.Combine(Path, "steamapps");

    public long FreeBytes
    {
        get
        {
            try { return new DriveInfo(System.IO.Path.GetPathRoot(Path)!).AvailableFreeSpace; }
            catch { return 0; }
        }
    }

    public override string ToString() => $"{Path}   ({Util.Size(FreeBytes)} free)";
}

public sealed record SteamGame(string AppId, string Name, string InstallDir, long SizeOnDisk, int StateFlags, SteamLibrary Library)
{
    public bool FullyInstalled => StateFlags == 4;
}

/// <summary>Finds Steam, its library folders and installed games.</summary>
public static class Steam
{
    public static string? RootPath
    {
        get
        {
            var p = Registry.GetValue(@"HKEY_CURRENT_USER\Software\Valve\Steam", "SteamPath", null) as string
                 ?? Registry.GetValue(@"HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath", null) as string
                 ?? Registry.GetValue(@"HKEY_LOCAL_MACHINE\SOFTWARE\Valve\Steam", "InstallPath", null) as string;
            if (string.IsNullOrWhiteSpace(p)) return null;
            p = System.IO.Path.GetFullPath(p.Replace('/', '\\'));
            return Directory.Exists(p) ? p : null;
        }
    }

    public static string? ExePath
    {
        get
        {
            var root = RootPath;
            if (root == null) return null;
            var exe = System.IO.Path.Combine(root, "steam.exe");
            return File.Exists(exe) ? exe : null;
        }
    }

    public static bool IsRunning => Process.GetProcessesByName("steam").Length > 0;

    static readonly Dictionary<string, string?> _artCache = new();

    /// <summary>Finds cached library artwork. kind: "capsule" (portrait), "hero" (wide banner), "header".</summary>
    public static string? FindArt(string appId, string kind)
    {
        if (!appId.All(char.IsAsciiDigit)) return null;
        var key = appId + "/" + kind;
        lock (_artCache) if (_artCache.TryGetValue(key, out var hit)) return hit;

        string? found = null;
        var root = RootPath;
        if (root != null)
        {
            var cache = System.IO.Path.Combine(root, "appcache", "librarycache");
            string[] names = kind switch
            {
                "capsule" => new[] { "library_600x900_2x.jpg", "library_600x900.jpg", "library_capsule.jpg" },
                "hero" => new[] { "library_hero.jpg", "library_header.jpg", "header.jpg" },
                _ => new[] { "library_header.jpg", "header.jpg" },
            };
            foreach (var n in names) // old flat layout: <appid>_library_600x900.jpg
            {
                var p = System.IO.Path.Combine(cache, $"{appId}_{n}");
                if (File.Exists(p)) { found = p; break; }
            }
            var dir = System.IO.Path.Combine(cache, appId);
            if (found == null && Directory.Exists(dir)) // new layout: <appid>/<hash>/library_capsule.jpg
            {
                var files = Directory.EnumerateFiles(dir, "*", SearchOption.AllDirectories).ToList();
                foreach (var n in names)
                {
                    found = files.FirstOrDefault(f => System.IO.Path.GetFileName(f).Equals(n, StringComparison.OrdinalIgnoreCase));
                    if (found != null) break;
                }
            }
        }
        lock (_artCache) _artCache[key] = found;
        return found;
    }

    public static List<SteamLibrary> GetLibraries()
    {
        var result = new List<SteamLibrary>();
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        void Add(string p)
        {
            try
            {
                p = System.IO.Path.GetFullPath(p.Replace('/', '\\')).TrimEnd('\\');
                if (Directory.Exists(p) && seen.Add(p)) result.Add(new SteamLibrary(p));
            }
            catch { }
        }

        var root = RootPath;
        if (root == null) return result;
        Add(root);

        var vdfPath = System.IO.Path.Combine(root, "steamapps", "libraryfolders.vdf");
        if (File.Exists(vdfPath))
        {
            var vdf = Vdf.Parse(File.ReadAllText(vdfPath));
            if (Vdf.Get(vdf, "libraryfolders") is Dictionary<string, object> libs)
            {
                foreach (var (_, v) in libs)
                {
                    if (v is Dictionary<string, object> lib && Vdf.Get(lib, "path") is string lp) Add(lp);
                    else if (v is string s && (s.Contains('\\') || s.Contains('/'))) Add(s); // legacy format
                }
            }
        }
        return result;
    }

    public static List<SteamGame> GetGames(IEnumerable<SteamLibrary>? libraries = null)
    {
        var games = new List<SteamGame>();
        foreach (var lib in libraries ?? GetLibraries())
        {
            if (!Directory.Exists(lib.SteamApps)) continue;
            foreach (var acf in Directory.EnumerateFiles(lib.SteamApps, "appmanifest_*.acf"))
            {
                try
                {
                    var g = ReadManifest(acf, lib);
                    if (g != null) games.Add(g);
                }
                catch { /* skip unreadable manifest */ }
            }
        }
        return games.OrderBy(g => g.Name, StringComparer.OrdinalIgnoreCase).ToList();
    }

    static SteamGame? ReadManifest(string acf, SteamLibrary lib)
    {
        var vdf = Vdf.Parse(File.ReadAllText(acf));
        if (Vdf.Get(vdf, "AppState") is not Dictionary<string, object> st) return null;
        var appId = Vdf.Get(st, "appid") as string;
        var name = Vdf.Get(st, "name") as string;
        var dir = Vdf.Get(st, "installdir") as string;
        if (appId == null || dir == null) return null;
        long.TryParse(Vdf.Get(st, "SizeOnDisk") as string, out var size);
        int.TryParse(Vdf.Get(st, "StateFlags") as string, out var flags);
        return new SteamGame(appId, name ?? dir, dir, size, flags, lib);
    }

    /// <summary>Returns the library that already has this app installed, if any.</summary>
    public static SteamLibrary? FindInstalled(string appId, IEnumerable<SteamLibrary> libs) =>
        libs.FirstOrDefault(l => File.Exists(System.IO.Path.Combine(l.SteamApps, $"appmanifest_{appId}.acf")));

    /// <summary>Steam stores the sender's steam.exe path in the manifest; point it at ours.</summary>
    public static void FixManifest(string acfPath)
    {
        var exe = ExePath;
        if (exe == null || !File.Exists(acfPath)) return;
        var text = File.ReadAllText(acfPath);
        var escaped = exe.Replace("\\", "\\\\");
        var fixedText = Regex.Replace(text, "(\"LauncherPath\"\\s+)\"[^\"]*\"", m => m.Groups[1].Value + "\"" + escaped + "\"");
        if (fixedText != text) File.WriteAllText(acfPath, fixedText, new UTF8Encoding(false));
    }

    public static async Task ShutdownAsync(Action<string> log)
    {
        if (!IsRunning) return;
        var exe = ExePath;
        log("Closing Steam...");
        if (exe != null) Process.Start(new ProcessStartInfo(exe, "-shutdown") { UseShellExecute = false });
        for (int i = 0; i < 60 && IsRunning; i++) await Task.Delay(500);
        if (IsRunning)
        {
            log("Steam didn't close in time; killing it.");
            foreach (var p in Process.GetProcessesByName("steam")) { try { p.Kill(); } catch { } }
            await Task.Delay(1500);
        }
    }

    public static async Task RestartAsync(Action<string> log)
    {
        await ShutdownAsync(log);
        var exe = ExePath;
        if (exe == null) { log("Steam not found."); return; }
        log("Starting Steam...");
        Process.Start(new ProcessStartInfo(exe) { UseShellExecute = true });
    }
}

/// <summary>Minimal Valve KeyValues (VDF/ACF) text parser.</summary>
public static class Vdf
{
    public static Dictionary<string, object> Parse(string text)
    {
        int i = 0;
        return ParseObject(text, ref i);
    }

    public static object? Get(Dictionary<string, object> d, string key) => d.TryGetValue(key, out var v) ? v : null;

    static Dictionary<string, object> ParseObject(string s, ref int i)
    {
        var d = new Dictionary<string, object>(StringComparer.OrdinalIgnoreCase);
        while (true)
        {
            var key = Next(s, ref i);
            if (key == null || key is { Quoted: false, Text: "}" }) return d;
            var val = Next(s, ref i);
            if (val == null) return d;
            if (val is { Quoted: false, Text: "{" }) d[key.Value.Text] = ParseObject(s, ref i);
            else d[key.Value.Text] = val.Value.Text;
            // skip optional conditional like [$WIN32]
            int save = i;
            var peek = Next(s, ref i);
            if (peek is not { Quoted: false } p || !p.Text.StartsWith('[')) i = save;
        }
    }

    static (string Text, bool Quoted)? Next(string s, ref int i)
    {
        while (i < s.Length)
        {
            if (char.IsWhiteSpace(s[i])) { i++; continue; }
            if (s[i] == '/' && i + 1 < s.Length && s[i + 1] == '/') { while (i < s.Length && s[i] != '\n') i++; continue; }
            break;
        }
        if (i >= s.Length) return null;
        char c = s[i];
        if (c == '{' || c == '}') { i++; return (c.ToString(), false); }
        var sb = new StringBuilder();
        if (c == '"')
        {
            i++;
            while (i < s.Length && s[i] != '"')
            {
                if (s[i] == '\\' && i + 1 < s.Length)
                {
                    i++;
                    sb.Append(s[i] switch { 'n' => '\n', 't' => '\t', _ => s[i] });
                }
                else sb.Append(s[i]);
                i++;
            }
            i++;
            return (sb.ToString(), true);
        }
        while (i < s.Length && !char.IsWhiteSpace(s[i]) && s[i] != '{' && s[i] != '}' && s[i] != '"') sb.Append(s[i++]);
        return (sb.ToString(), false);
    }
}
