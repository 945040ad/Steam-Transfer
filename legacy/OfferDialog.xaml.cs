using System.Windows;
using System.Windows.Media;

namespace SteamTransfer;

/// <summary>"PC X wants to send you Game Y — install where?" (also used for installing from a drive).</summary>
public partial class OfferDialog : Window
{
    readonly Offer _offer;
    public ReceiveDecision? Decision { get; private set; }

    public OfferDialog(Offer offer, string from, bool incoming, List<SteamLibrary> libs)
    {
        InitializeComponent();
        _offer = offer;
        Title = incoming ? "Incoming game" : "Install from drive";
        Kicker.Text = (incoming ? "INCOMING FROM " : "INSTALL FROM ") + from.ToUpperInvariant();
        GameTitle.Text = offer.Name;
        Sub.Text = $"{Util.Size(offer.TotalBytes)} · {offer.Files.Count:N0} files";
        NoButton.Content = incoming ? "Decline" : "Cancel";
        YesButton.Content = incoming ? "Accept" : "Install";

        HeroFallback.Background = Art.PlaceholderBrush(offer.Name);
        Art.Load(offer.AppId, "hero", 1000, img => HeroImage.Background = new ImageBrush(img) { Stretch = Stretch.UniformToFill });

        var existing = Steam.FindInstalled(offer.AppId, libs);
        ExistingNote.Visibility = existing != null ? Visibility.Visible : Visibility.Collapsed;
        var items = libs.Select(l => new LibraryItem { Library = l, HasGame = l == existing }).ToList();
        Libraries.ItemsSource = items;
        Libraries.SelectedItem = items.FirstOrDefault(i => i.HasGame) ?? items.OrderByDescending(i => i.Library.FreeBytes).First();

        CloseSteam.Visibility = Steam.IsRunning ? Visibility.Visible : Visibility.Collapsed;
        CloseSteam.IsChecked = Steam.IsRunning;
    }

    async void Yes_Click(object sender, RoutedEventArgs e)
    {
        if (Libraries.SelectedItem is not LibraryItem item) return;
        YesButton.IsEnabled = false;
        ErrorNote.Visibility = Visibility.Collapsed;
        var lib = item.Library;
        var need = await Task.Run(() =>
            _offer.TotalBytes - Transfer.ComputeSkip(_offer, lib.Path).Sum(i => _offer.Files[i].Size) + 256L * 1024 * 1024);
        if (lib.FreeBytes < need)
        {
            ErrorText.Text = $"Not enough space on {lib.Path}: needs {Util.Size(need)}, only {Util.Size(lib.FreeBytes)} free.";
            ErrorNote.Visibility = Visibility.Visible;
            YesButton.IsEnabled = true;
            return;
        }
        Decision = new ReceiveDecision { Library = lib, CloseSteam = CloseSteam.IsChecked == true && CloseSteam.Visibility == Visibility.Visible };
        DialogResult = true;
    }

    void No_Click(object sender, RoutedEventArgs e) => DialogResult = false;
}
