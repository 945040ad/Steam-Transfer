using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;

namespace SteamTransfer;

public abstract class Observable : INotifyPropertyChanged
{
    public event PropertyChangedEventHandler? PropertyChanged;
    protected void Set<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value)) return;
        field = value;
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
    }
}

/// <summary>Cover art: Steam's local cache first, then the Steam CDN; a generated tile shows until/unless one loads.</summary>
public static class Art
{
    const string Cdn = "https://shared.cloudflare.steamstatic.com/store_item_assets/steam/apps/";

    public static void Load(string appId, string kind, int decodeWidth, Action<ImageSource> apply)
    {
        _ = Task.Run(() =>
        {
            var file = Steam.FindArt(appId, kind);
            if (file != null)
            {
                try
                {
                    var bi = new BitmapImage();
                    bi.BeginInit();
                    bi.CacheOption = BitmapCacheOption.OnLoad;
                    bi.UriSource = new Uri(file);
                    bi.DecodePixelWidth = decodeWidth;
                    bi.EndInit();
                    bi.Freeze();
                    Application.Current?.Dispatcher.BeginInvoke(() => apply(bi));
                    return;
                }
                catch { }
            }
            // Not cached locally: try the CDN (silently does nothing when offline, e.g. on a direct cable).
            Application.Current?.Dispatcher.BeginInvoke(() =>
            {
                try
                {
                    var url = Cdn + appId + (kind == "capsule" ? "/library_600x900.jpg" : "/library_hero.jpg");
                    var bi = new BitmapImage();
                    bi.BeginInit();
                    bi.UriSource = new Uri(url);
                    bi.DecodePixelWidth = decodeWidth;
                    bi.EndInit();
                    bi.DownloadCompleted += (_, _) => apply(bi);
                }
                catch { }
            });
        });
    }

    public static Brush PlaceholderBrush(string name)
    {
        int h = 0;
        foreach (var c in name) h = (h * 31 + c) % 360;
        var b = new LinearGradientBrush(Hsl(h, .55, .40), Hsl((h + 50) % 360, .60, .22), 50);
        b.Freeze();
        return b;
    }

    public static string Initials(string name)
    {
        var words = name.Split(' ', StringSplitOptions.RemoveEmptyEntries)
            .Select(w => new string(w.Where(char.IsLetterOrDigit).ToArray())).Where(w => w.Length > 0).Take(2);
        var s = string.Concat(words.Select(w => char.ToUpperInvariant(w[0])));
        return s.Length > 0 ? s : "?";
    }

    static Color Hsl(double h, double s, double l)
    {
        double c = (1 - Math.Abs(2 * l - 1)) * s, x = c * (1 - Math.Abs(h / 60 % 2 - 1)), m = l - c / 2;
        var (r, g, b) = h switch { < 60 => (c, x, 0d), < 120 => (x, c, 0d), < 180 => (0d, c, x), < 240 => (0d, x, c), < 300 => (x, 0d, c), _ => (c, 0d, x) };
        return Color.FromRgb((byte)((r + m) * 255), (byte)((g + m) * 255), (byte)((b + m) * 255));
    }
}

public sealed class GameItem : Observable
{
    ImageSource? _art;
    public GameItem(SteamGame g)
    {
        Game = g;
        Placeholder = Art.PlaceholderBrush(g.Name);
        Art.Load(g.AppId, "capsule", 300, img => ArtImage = img);
    }
    public SteamGame Game { get; }
    public string Name => Game.Name;
    public string Meta => (Game.SizeOnDisk > 0 ? Util.Size(Game.SizeOnDisk) : "—") + " · " + Game.Library.Path[..2].ToUpperInvariant();
    public Visibility UpdateBadge => Game.FullyInstalled ? Visibility.Collapsed : Visibility.Visible;
    public Brush Placeholder { get; }
    public string Initials => Art.Initials(Game.Name);
    public ImageSource? ArtImage { get => _art; private set => Set(ref _art, value); }
}

public sealed class PeerItem
{
    public required Peer Peer { get; init; }
    public string Key => Peer.Key;
    public string Name => Peer.Name;
    public string Glyph => Peer.Kind switch { "usb4" => "", "wifi" => "", _ => "" };
    public string Subtitle => $"{KindName}{(Peer.LinkBps > 0 ? " · " + Link(Peer.LinkBps) : "")} · {Peer.Address}";
    public string KindName => Peer.Kind switch { "usb4" => "USB4 / Thunderbolt", "wifi" => "Wi-Fi", _ => "Ethernet" };
    public string Tag { get; init; } = "";
    public Visibility TagVisibility => Tag.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    public static string Link(long bps) => bps >= 1_000_000_000 ? $"{bps / 1e9:0.#} Gbps" : $"{bps / 1e6:0} Mbps";
}

public sealed class DriveItem
{
    public required DriveEntry Drive { get; init; }
    public string Root => Drive.Root;
    public string Title => $"{Drive.Label} ({Drive.Root.TrimEnd('\\')})";
    public string Glyph => Drive.Removable ? "" : "";
    public bool TooSmall { get; init; }
    public string Subtitle => $"{Util.Size(Drive.Free)} free of {Util.Size(Drive.Total)}" + (TooSmall ? " · not enough space" : "");
    public double UsedPercent => Drive.Total > 0 ? 100.0 * (Drive.Total - Drive.Free) / Drive.Total : 0;
}

public sealed class PackageItem : Observable
{
    ImageSource? _art;
    public PackageItem(PackageInfo p, string where)
    {
        Package = p;
        Where = where;
        Placeholder = Art.PlaceholderBrush(p.Name);
        Art.Load(p.AppId, "capsule", 120, img => ArtImage = img);
    }
    public PackageInfo Package { get; }
    public string Where { get; }
    public string Name => Package.Name;
    public string Subtitle => $"{Util.Size(Package.Size)} · on {Where}";
    public Brush Placeholder { get; }
    public string Initials => Art.Initials(Package.Name);
    public ImageSource? ArtImage { get => _art; private set => Set(ref _art, value); }
}

public sealed class LibraryItem
{
    public required SteamLibrary Library { get; init; }
    public bool HasGame { get; init; }
    public string Path => Library.Path;
    public string Subtitle => $"{Util.Size(Library.FreeBytes)} free" + (HasGame ? " · already has this game" : "");
}
