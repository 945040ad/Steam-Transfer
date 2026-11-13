using System.Diagnostics;
using System.Net;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Threading;
using Microsoft.Win32;

namespace SteamTransfer;

public partial class MainWindow : Window, ITransferHost
{
    readonly DispatcherTimer _timer = new() { Interval = TimeSpan.FromMilliseconds(250) };
    readonly Discovery _discovery = new();
    readonly Server _server;
    readonly List<Peer> _manualPeers = new();
    readonly object _jobLock = new();

    List<SteamGame> _games = new();
    List<GameItem> _gameItems = new();
    CancellationTokenSource? _jobCts;
    TransferProgress? _progress;
    string _jobTitle = "";
    bool _cancelRequested, _installHidden, _userPickedTarget, _syncingTargets;
    bool? _firewallOk;
    double _speed; long _lastBytes; double _lastSecs;
    string _peersKey = "", _drivesKey = "", _stateKey = "", _lastWaiting = "";
    List<DriveEntry> _drives = new();
    readonly List<PackageItem> _browsedPackages = new();
    int _tick;

    public MainWindow()
    {
        InitializeComponent();
        MachineText.Text = Environment.MachineName;
        VersionText.Text = "v" + Installer.Version;
        _server = new Server(this);
        _timer.Tick += (_, _) => OnTick();
        Loaded += (_, _) => StartUp();
        Closing += OnClosing;
    }

    // ================= startup / shutdown =================

    void StartUp()
    {
        LoadGames();
        try { _server.Start(); _discovery.Start(); }
        catch (Exception ex) { Log($"Network error: {ex.Message}. Is another copy of this app already running?"); }
        _timer.Start();
        RenderTargets();
        PushState();

        _ = Task.Run(() =>
        {
            Hardware.HasUsb4Controller();
            _firewallOk = Firewall.RuleExists();
            if (_firewallOk == false && Installer.IsRunningInstalled)
            {
                Log("Setting up Windows Firewall so other PCs can connect (approve the prompt once)...");
                _firewallOk = Firewall.AddRules();
            }
            Dispatcher.BeginInvoke(() => { _stateKey = ""; PushState(); });
            var drives = Hardware.Drives();
            Dispatcher.BeginInvoke(() => SetDrives(drives));
        });
    }

    void OnClosing(object? sender, System.ComponentModel.CancelEventArgs e)
    {
        if (_jobCts != null && MessageBox.Show(this, "A transfer is running. Quit anyway?\nYou can resume it later — finished files are kept.",
                Title, MessageBoxButton.YesNo, MessageBoxImage.Warning) != MessageBoxResult.Yes)
        { e.Cancel = true; return; }
        _jobCts?.Cancel();
        _discovery.Dispose();
        _server.Dispose();
    }

    void OnTick()
    {
        _tick++;
        UpdateProgress();
        if (_tick % 4 == 0) RenderPeers();
        if (_tick % 12 == 0)
        {
            PushState();
            _ = Task.Run(() => { var d = Hardware.Drives(); Dispatcher.BeginInvoke(() => SetDrives(d)); });
        }
    }

    // ================= status strip =================

    void PushState()
    {
        var links = Hardware.Links();
        var steam = Steam.RootPath != null;
        var steamRunning = Steam.IsRunning;
        var key = JsonSerializer.Serialize(new { links, steam, steamRunning, _firewallOk, Installer.IsRunningInstalled, _installHidden });
        if (key == _stateKey) return;
        _stateKey = key;

        InstallBar.Visibility = !Installer.IsRunningInstalled && !_installHidden ? Visibility.Visible : Visibility.Collapsed;
        Chips.Children.Clear();
        Chips.Children.Add(Chip(steam ? (steamRunning ? "Steam running" : "Steam") : "Steam not found", steam ? "SystemFillColorSuccessBrush" : "SystemFillColorCriticalBrush"));
        if (_firewallOk == false)
        {
            var chip = Chip("Firewall: click to fix", "SystemFillColorCautionBrush");
            chip.Cursor = Cursors.Hand;
            chip.ToolTip = "Allow other PCs to connect to this one";
            chip.MouseLeftButtonUp += (_, _) => FixFirewall();
            Chips.Children.Add(chip);
        }
        else if (_firewallOk == true) Chips.Children.Add(Chip("Firewall ready", "SystemFillColorSuccessBrush"));

        var usb = links.Where(l => l.Kind == "usb4").ToList();
        var usbUp = usb.FirstOrDefault(l => l.Up);
        if (usbUp != null) Chips.Children.Add(Chip("USB4 link " + PeerItem.Link(usbUp.Bps), null, ""));
        else if (usb.Count > 0) Chips.Children.Add(Chip("USB4 ready · no cable", null, ""));
        else if (Hardware.HasUsb4Controller()) Chips.Children.Add(Chip("USB4 port", null, ""));

        var eth = links.Where(l => l.Kind == "ethernet").ToList();
        var ethUp = eth.Where(l => l.Up).OrderByDescending(l => l.Bps).FirstOrDefault();
        if (ethUp != null) Chips.Children.Add(Chip("Ethernet " + PeerItem.Link(ethUp.Bps), "SystemFillColorSuccessBrush"));
        else if (eth.Count > 0) Chips.Children.Add(Chip("Ethernet unplugged", null));
        var wifi = links.FirstOrDefault(l => l.Kind == "wifi" && l.Up);
        if (wifi != null) Chips.Children.Add(Chip("Wi-Fi " + PeerItem.Link(wifi.Bps), null, ""));

        TipUsb4.Visibility = Hardware.HasUsb4Controller() ? Visibility.Visible : Visibility.Collapsed;
    }

    Border Chip(string text, string? dotBrush, string? glyph = null)
    {
        var sp = new StackPanel { Orientation = Orientation.Horizontal };
        if (glyph != null)
        {
            var g = new TextBlock { Text = glyph, FontFamily = (FontFamily)FindResource("Icons"), FontSize = 12, Margin = new Thickness(0, 0, 6, 0), VerticalAlignment = VerticalAlignment.Center };
            g.SetResourceReference(TextBlock.ForegroundProperty, "AccentTextFillColorPrimaryBrush");
            sp.Children.Add(g);
        }
        else
        {
            var dot = new System.Windows.Shapes.Ellipse { Width = 8, Height = 8, Margin = new Thickness(0, 0, 7, 0), VerticalAlignment = VerticalAlignment.Center };
            dot.SetResourceReference(System.Windows.Shapes.Shape.FillProperty, dotBrush ?? "TextFillColorTertiaryBrush");
            sp.Children.Add(dot);
        }
        var t = new TextBlock { Text = text, FontSize = 12, VerticalAlignment = VerticalAlignment.Center };
        sp.Children.Add(t);
        return new Border { Style = (Style)FindResource("Chip"), Child = sp };
    }

    void FixFirewall() => _ = Task.Run(() =>
    {
        _firewallOk = Firewall.AddRules();
        Dispatcher.BeginInvoke(() => { _stateKey = ""; PushState(); });
    });

    // ================= games =================

    void LoadGames()
    {
        _ = Task.Run(() =>
        {
            var games = Steam.RootPath == null ? new List<SteamGame>() : Steam.GetGames().Where(g => g.AppId != "228980").ToList();
            Dispatcher.BeginInvoke(() =>
            {
                var selected = SelectedGame?.AppId;
                _games = games;
                _gameItems = games.Select(g => new GameItem(g)).ToList();
                CountRun.Text = games.Count > 0 ? "  " + games.Count : "";
                ApplySearch();
                if (selected != null) GamesList.SelectedItem = _gameItems.FirstOrDefault(i => i.Game.AppId == selected);
            });
        });
    }

    void ApplySearch()
    {
        var q = SearchBox.Text.Trim();
        var list = q.Length == 0 ? _gameItems : _gameItems.Where(g => g.Name.Contains(q, StringComparison.OrdinalIgnoreCase)).ToList();
        var selected = GamesList.SelectedItem;
        GamesList.ItemsSource = list;
        if (selected != null && list.Contains(selected)) GamesList.SelectedItem = selected;
        bool noSteam = Steam.RootPath == null;
        GamesEmpty.Visibility = list.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        GamesEmptyTitle.Text = noSteam ? "Steam isn't installed on this PC" : _gameItems.Count > 0 ? "No matches" : "No installed games yet";
        GamesEmptyText.Text = noSteam ? "You can still receive games once Steam is installed." : _gameItems.Count > 0 ? "Try a different search." : "Games you receive will show up here.";
    }

    SteamGame? SelectedGame => (GamesList.SelectedItem as GameItem)?.Game;

    void Search_Changed(object sender, TextChangedEventArgs e)
    {
        SearchHint.Visibility = SearchBox.Text.Length == 0 ? Visibility.Visible : Visibility.Collapsed;
        ApplySearch();
    }

    void Refresh_Click(object sender, RoutedEventArgs e) => LoadGames();

    void Games_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        var g = SelectedGame;
        HeroImage.Background = null;
        if (g == null)
        {
            HeroShade.Visibility = Visibility.Collapsed;
            HeroHint.Visibility = Visibility.Visible;
        }
        else
        {
            HeroHint.Visibility = Visibility.Collapsed;
            HeroShade.Visibility = Visibility.Visible;
            HeroFallback.Background = Art.PlaceholderBrush(g.Name);
            HeroTitle.Text = g.Name;
            HeroSub.Text = (g.SizeOnDisk > 0 ? Util.Size(g.SizeOnDisk) + " · " : "") + g.Library.Path;
            var appId = g.AppId;
            Art.Load(appId, "hero", 800, img =>
            {
                if (SelectedGame?.AppId == appId) HeroImage.Background = new ImageBrush(img) { Stretch = Stretch.UniformToFill };
            });
        }
        if (g == null) HeroFallback.SetResourceReference(Border.BackgroundProperty, "CardBackgroundFillColorDefaultBrush");
        RenderDrives();
        RenderAction();
    }

    // ================= targets =================

    List<Peer> AllPeers()
    {
        var found = _discovery.Peers;
        return found.Concat(_manualPeers.Where(m => found.All(d => !d.Address.Equals(m.Address)))).ToList();
    }

    void RenderTargets() { RenderPeers(); RenderDrives(); RenderPackages(); }

    void RenderPeers()
    {
        var peers = AllPeers();
        var key = string.Join("|", peers.Select(p => p.Key + p.Name + p.Kind + p.LinkBps));
        if (key == _peersKey) return;
        _peersKey = key;

        var selectedKey = (PeersList.SelectedItem as PeerItem)?.Key;
        var items = peers.Select((p, i) => new PeerItem
        {
            Peer = p,
            Tag = p.Kind == "usb4" ? "USB4" : p.Kind == "wifi" ? "SLOW" : i == 0 && peers.Count > 1 ? "FASTEST" : "",
        }).ToList();
        _syncingTargets = true;
        PeersList.ItemsSource = items;
        var keep = items.FirstOrDefault(i => i.Key == selectedKey);
        if (keep != null) PeersList.SelectedItem = keep;
        else if (!_userPickedTarget && items.Count > 0 && DrivesList.SelectedItem == null) PeersList.SelectedItem = items[0]; // fastest first
        _syncingTargets = false;
        PeersList.Visibility = items.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        PeersEmpty.Visibility = items.Count > 0 ? Visibility.Collapsed : Visibility.Visible;
        RenderAction();
    }

    void SetDrives(List<DriveEntry> drives)
    {
        var key = JsonSerializer.Serialize(drives);
        if (key == _drivesKey) return;
        _drivesKey = key;
        _drives = drives;
        RenderDrives();
        RenderPackages();
    }

    void RenderDrives()
    {
        var g = SelectedGame;
        var selectedRoot = (DrivesList.SelectedItem as DriveItem)?.Root;
        var items = _drives.Select(d => new DriveItem { Drive = d, TooSmall = g != null && g.SizeOnDisk > 0 && d.Free < g.SizeOnDisk }).ToList();
        _syncingTargets = true;
        DrivesList.ItemsSource = items;
        var keep = items.FirstOrDefault(i => i.Root == selectedRoot && !i.TooSmall);
        if (keep != null) DrivesList.SelectedItem = keep;
        _syncingTargets = false;
        DrivesList.Visibility = items.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        DrivesEmpty.Visibility = items.Count > 0 ? Visibility.Collapsed : Visibility.Visible;
        RenderAction();
    }

    void RenderPackages()
    {
        var items = _drives.SelectMany(d => d.Packages.Select(p => new PackageItem(p, d.Root.TrimEnd('\\'))))
            .Concat(_browsedPackages)
            .GroupBy(p => p.Package.Folder, StringComparer.OrdinalIgnoreCase).Select(gr => gr.First()).ToList();
        PackagesList.ItemsSource = items;
        PackagesEmpty.Visibility = items.Count > 0 ? Visibility.Collapsed : Visibility.Visible;
    }

    void Target_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_syncingTargets) return;
        if (sender is ListBox lb && lb.SelectedItem != null)
        {
            _userPickedTarget = true;
            _syncingTargets = true;
            if (lb == PeersList) DrivesList.SelectedItem = null; else PeersList.SelectedItem = null;
            _syncingTargets = false;
        }
        RenderAction();
    }

    void RenderAction()
    {
        var g = SelectedGame;
        var peer = PeersList.SelectedItem as PeerItem;
        var drive = DrivesList.SelectedItem as DriveItem;
        bool busy = _jobCts != null;
        string text, label = "Send", glyph = "";
        bool ok = false;
        if (busy) text = "A transfer is in progress…";
        else if (g == null) text = "Pick a game to send.";
        else if (peer != null)
        {
            label = $"Send to {peer.Name}";
            ok = true;
            var bps = peer.Peer.LinkBps > 0 ? peer.Peer.LinkBps : 1_000_000_000;
            var secs = g.SizeOnDisk / Math.Min(bps / 8.0 * 0.9, 2.5e9);
            text = $"{g.Name} → {peer.Name} via {peer.KindName}" + (g.SizeOnDisk > 0 ? $" · about {Util.Time(TimeSpan.FromSeconds(secs))}" : "");
        }
        else if (drive != null)
        {
            label = $"Copy to {drive.Root.TrimEnd('\\')}";
            glyph = "";
            ok = !drive.TooSmall;
            text = $"{g.Name} → {drive.Title}";
        }
        else text = $"Pick a PC or drive for {g.Name}.";

        GoText.Text = label;
        GoGlyph.Text = glyph;
        GoButton.IsEnabled = ok && !busy;
        SummaryText.Text = text;
    }

    void PassWheel(object sender, MouseWheelEventArgs e)
    {
        // Lists in the side panel don't scroll themselves; let the panel scroll.
        e.Handled = true;
        SideScroll.ScrollToVerticalOffset(SideScroll.VerticalOffset - e.Delta / 3.0);
    }

    void Ip_Changed(object sender, TextChangedEventArgs e) => IpHint.Visibility = IpBox.Text.Length == 0 ? Visibility.Visible : Visibility.Collapsed;
    void Ip_KeyDown(object sender, KeyEventArgs e) { if (e.Key == Key.Enter) AddPeer_Click(sender, e); }

    void AddPeer_Click(object sender, RoutedEventArgs e)
    {
        if (!IPAddress.TryParse(IpBox.Text.Trim(), out var ip)) { Toast("That's not a valid IP address.", "error"); return; }
        var r = Discovery.LocalRouteTo(ip);
        _manualPeers.RemoveAll(p => p.Address.Equals(ip));
        _manualPeers.Add(new Peer(ip.ToString(), ip, r.Via, r.Bps, r.Kind, true));
        IpBox.Clear();
        _peersKey = "";
        RenderPeers();
    }

    // ================= actions =================

    async void Go_Click(object sender, RoutedEventArgs e)
    {
        var g = SelectedGame;
        if (g == null) return;
        if (!g.FullyInstalled && MessageBox.Show(this, $"{g.Name} isn't fully up to date on this PC. Send it anyway?\nSteam will finish the update on the other side.",
                Title, MessageBoxButton.YesNo, MessageBoxImage.Question) != MessageBoxResult.Yes) return;
        if (PeersList.SelectedItem is PeerItem peer) await SendGame(g, peer.Peer);
        else if (DrivesList.SelectedItem is DriveItem drive) await ExportGame(g, drive.Root);
    }

    async Task SendGame(SteamGame g, Peer peer)
    {
        var job = BeginJob($"Sending {g.Name} to {peer.Name}");
        if (job == null) return;
        try
        {
            await Sender.SendAsync(g, peer.Address, job.Value.Progress, Log, job.Value.Token);
            EndJob(null, $"{g.Name} is on {peer.Name}. It shows up in Steam there.");
        }
        catch (Exception ex) { EndJob(ex, null); }
    }

    async Task ExportGame(SteamGame g, string root)
    {
        var job = BeginJob($"Copying {g.Name} to {root.TrimEnd('\\')}");
        if (job == null) return;
        try
        {
            await Task.Run(() => Transfer.ExportAsync(g, root, job.Value.Progress, Log, job.Value.Token));
            Installer.CopyAppTo(Path.Combine(root, "SteamTransfer"));
            EndJob(null, $"{g.Name} is on the drive. Plug it into the other PC and open SteamTransfer\\SteamTransfer.exe there.");
            _drivesKey = "";
            _ = Task.Run(() => { var d = Hardware.Drives(); Dispatcher.BeginInvoke(() => SetDrives(d)); });
        }
        catch (Exception ex) { EndJob(ex, null); }
    }

    async void BrowseExport_Click(object sender, RoutedEventArgs e)
    {
        var g = SelectedGame;
        if (g == null) { Toast("Pick a game first."); return; }
        var dlg = new OpenFolderDialog { Title = "Where should the game be copied to?" };
        if (dlg.ShowDialog(this) == true) await ExportGame(g, dlg.FolderName);
    }

    void BrowseImport_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new OpenFolderDialog { Title = "Pick the drive or folder that has the copied game" };
        if (dlg.ShowDialog(this) != true) return;
        var pkgs = Hardware.FindPackages(dlg.FolderName);
        if (pkgs.Count == 0) { Toast("No copied games found in that folder.", "error"); return; }
        foreach (var p in pkgs) _browsedPackages.Add(new PackageItem(p, Path.GetPathRoot(p.Folder)?.TrimEnd('\\') ?? p.Folder));
        RenderPackages();
        Toast($"Found {pkgs.Count} game{(pkgs.Count > 1 ? "s" : "")} — click Install.", "ok");
    }

    async void ImportPackage_Click(object sender, RoutedEventArgs e)
    {
        if ((sender as FrameworkElement)?.Tag is not PackageItem item) return;
        var folder = item.Package.Folder;
        Offer offer;
        try { offer = JsonSerializer.Deserialize<Offer>(File.ReadAllText(Path.Combine(folder, Transfer.PackageFile)))!; }
        catch (Exception ex) { Toast("Couldn't read that package: " + ex.Message, "error"); return; }

        var decision = AskUser(offer, "drive " + Path.GetPathRoot(folder)?.TrimEnd('\\'), incoming: false);
        if (decision == null) return;
        var job = BeginJob($"Installing {offer.Name} from drive");
        if (job == null) return;
        bool closed = false;
        try
        {
            if (decision.CloseSteam && Steam.IsRunning) { await Steam.ShutdownAsync(Log); closed = true; }
            await Task.Run(() => Transfer.ImportAsync(offer, folder, decision.Library, job.Value.Progress, Log, job.Value.Token));
            EndJob(null, $"{offer.Name} is installed.");
            await AfterReceiveAsync(offer, closed);
        }
        catch (Exception ex)
        {
            EndJob(ex, null);
            if (closed) await Steam.RestartAsync(Log);
        }
    }

    async void Install_Click(object sender, RoutedEventArgs e)
    {
        try
        {
            Toast("Installing… approve the Windows prompt so other PCs can connect.");
            await Task.Run(Installer.Install);
            if (!Installer.IsRunningInstalled)
            {
                Installer.LaunchInstalled();
                _jobCts?.Cancel();
                Close();
                return;
            }
            _firewallOk = Firewall.RuleExists();
            _stateKey = "";
            PushState();
            Toast("Installed.", "ok");
        }
        catch (IOException) { Toast("The installed copy is already running. Close it and try again.", "error"); }
        catch (Exception ex) { Toast("Install failed: " + ex.Message, "error"); }
    }

    void HideInstall_Click(object sender, RoutedEventArgs e) { _installHidden = true; _stateKey = ""; PushState(); }

    void Pause_Click(object sender, RoutedEventArgs e)
    {
        if (_progress is not { } p) return;
        p.Pause();
        Log("Paused.");
        UpdateProgress();
    }

    void Resume_Click(object sender, RoutedEventArgs e)
    {
        if (_progress is not { } p) return;
        p.Resume();
        _speed = 0; _lastBytes = p.Done; _lastSecs = p.Clock.Elapsed.TotalSeconds;
        Log("Resumed.");
        UpdateProgress();
    }

    void Cancel_Click(object sender, RoutedEventArgs e)
    {
        _cancelRequested = true;
        _progress?.Resume();
        _jobCts?.Cancel();
        Log("Cancelling...");
    }

    void CloseTransfer_Click(object sender, RoutedEventArgs e) => TransferPanel.Visibility = Visibility.Collapsed;

    void LogToggle_Changed(object sender, RoutedEventArgs e) =>
        LogPanel.Visibility = LogToggle.IsChecked == true ? Visibility.Visible : Visibility.Collapsed;

    // ================= progress =================

    void UpdateProgress()
    {
        var p = _progress;
        if (p == null) return;
        var done = p.Done;
        var secs = p.Clock.Elapsed.TotalSeconds;
        var waiting = p.Waiting;
        if (waiting != null) { _speed = 0; _lastBytes = done; _lastSecs = secs; }
        else if (secs - _lastSecs >= 0.5)
        {
            var inst = (done - _lastBytes) / (secs - _lastSecs);
            _speed = _speed == 0 ? inst : _speed * 0.7 + inst * 0.3;
            _lastBytes = done; _lastSecs = secs;
        }

        TransferPanel.Visibility = Visibility.Visible;
        TTitle.Text = _jobTitle;
        var pct = p.Total > 0 ? Math.Min(100.0, done * 100.0 / p.Total) : 0;
        TBar.IsIndeterminate = p.Total == 0;
        TBar.Value = pct * 10;
        TPercent.Text = p.Total > 0 ? $"{(int)pct}%" : "";
        var size = p.Total > 0 ? $"{Util.Size(done)} of {Util.Size(p.Total)}" : "";

        if (waiting != null)
        {
            SetAlert("", "SystemFillColorCriticalBrush");
            TStats.Text = $"{waiting}   {size}";
            TBar.SetResourceReference(ForegroundProperty, "SystemFillColorCriticalBrush");
            if (waiting != _lastWaiting) Toast(waiting, "error");
        }
        else if (p.Paused)
        {
            SetAlert("", "SystemFillColorCautionBrush");
            TStats.Text = $"Paused   {size}";
            TBar.SetResourceReference(ForegroundProperty, "SystemFillColorCautionBrush");
        }
        else
        {
            TAlertGlyph.Visibility = Visibility.Collapsed;
            TStats.ClearValue(TextBlock.ForegroundProperty);
            TBar.ClearValue(ForegroundProperty);
            TStats.Text = p.Total > 0
                ? $"{size}   ·   {Util.Size((long)_speed)}/s ({_speed * 8 / 1e9:0.00} Gbps)   ·   {(_speed > 1 ? Util.Time(TimeSpan.FromSeconds((p.Total - done) / _speed)) + " left" : "—")}"
                : p.Current;
        }
        _lastWaiting = waiting ?? "";
        TFile.Text = p.Total > 0 ? p.Current : "";
        TPause.Visibility = p.Paused ? Visibility.Collapsed : Visibility.Visible;
        TResume.Visibility = p.Paused ? Visibility.Visible : Visibility.Collapsed;
        TCancel.Visibility = Visibility.Visible;
        TClose.Visibility = Visibility.Collapsed;
    }

    void SetAlert(string glyph, string brush)
    {
        TAlertGlyph.Text = glyph;
        TAlertGlyph.Visibility = Visibility.Visible;
        TAlertGlyph.SetResourceReference(TextBlock.ForegroundProperty, brush);
        TStats.SetResourceReference(TextBlock.ForegroundProperty, brush);
    }

    // ================= notifications =================

    void Toast(string text, string kind = "info") => Ui(() =>
    {
        var glyph = kind switch { "ok" => "", "error" => "", _ => "" };
        var brush = kind switch { "ok" => "SystemFillColorSuccessBrush", "error" => "SystemFillColorCriticalBrush", _ => "AccentTextFillColorPrimaryBrush" };
        var icon = new TextBlock { Text = glyph, FontFamily = (FontFamily)FindResource("Icons"), FontSize = 16, Margin = new Thickness(0, 0, 12, 0), VerticalAlignment = VerticalAlignment.Top };
        icon.SetResourceReference(TextBlock.ForegroundProperty, brush);
        var dock = new DockPanel();
        DockPanel.SetDock(icon, Dock.Left);
        dock.Children.Add(icon);
        dock.Children.Add(new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap });
        var card = new Border { Child = dock, Margin = new Thickness(0, 6, 0, 0), Padding = new Thickness(14, 12, 14, 12), CornerRadius = new CornerRadius(8), BorderThickness = new Thickness(1) };
        card.SetResourceReference(Border.BackgroundProperty, "SolidBackgroundFillColorBaseBrush");
        card.SetResourceReference(Border.BorderBrushProperty, "CardStrokeColorDefaultBrush");
        card.Effect = new System.Windows.Media.Effects.DropShadowEffect { BlurRadius = 18, ShadowDepth = 3, Opacity = 0.3 };
        Toasts.Children.Add(card);
        var t = new DispatcherTimer { Interval = TimeSpan.FromSeconds(kind == "error" ? 7 : 4.5) };
        t.Tick += (_, _) => { t.Stop(); Toasts.Children.Remove(card); };
        t.Start();
    });

    // ================= ITransferHost =================

    public void Log(string msg) => Ui(() =>
    {
        LogBox.AppendText($"[{DateTime.Now:HH:mm:ss}] {msg}{Environment.NewLine}");
        LogBox.ScrollToEnd();
        LastLog.Text = msg;
    });

    public (TransferProgress Progress, CancellationToken Token)? BeginJob(string title)
    {
        lock (_jobLock)
        {
            if (_jobCts != null) { Toast("Another transfer is still running.", "error"); return null; }
            _jobCts = new CancellationTokenSource();
            _progress = new TransferProgress();
            _jobTitle = title;
            _speed = 0; _lastBytes = 0; _lastSecs = 0;
            _cancelRequested = false;
            _lastWaiting = "";
        }
        Log(title + "...");
        Ui(() => { UpdateProgress(); RenderAction(); });
        return (_progress, _jobCts.Token);
    }

    public void EndJob(Exception? error, string? message)
    {
        var p = _progress;
        lock (_jobLock) { _jobCts?.Dispose(); _jobCts = null; _progress = null; }
        string status, text;
        if (error != null && (error is OperationCanceledException || _cancelRequested))
        {
            status = "cancelled";
            text = "Cancelled. Start it again to resume — files already copied are skipped.";
        }
        else if (error != null) { status = "error"; text = error.Message; }
        else
        {
            status = message == "Declined." ? "cancelled" : "ok";
            text = message ?? "Done.";
            var secs = p?.Clock.Elapsed.TotalSeconds ?? 0;
            if (p != null && p.Done > 0 && secs > 0) text += $"  ({Util.Size(p.Done)} in {Util.Time(p.Clock.Elapsed)}, avg {Util.Size((long)(p.Done / secs))}/s)";
        }
        Log((status == "error" ? "ERROR: " : "") + text);
        Ui(() =>
        {
            TransferPanel.Visibility = Visibility.Visible;
            TBar.IsIndeterminate = false;
            TAlertGlyph.Visibility = Visibility.Visible;
            if (status == "ok") { TBar.Value = TBar.Maximum; TPercent.Text = ""; SetAlert("", "SystemFillColorSuccessBrush"); TBar.SetResourceReference(ForegroundProperty, "SystemFillColorSuccessBrush"); }
            else if (status == "error") { SetAlert("", "SystemFillColorCriticalBrush"); TBar.SetResourceReference(ForegroundProperty, "SystemFillColorCriticalBrush"); }
            else { SetAlert("", "TextFillColorSecondaryBrush"); TBar.ClearValue(ForegroundProperty); }
            TStats.Text = text;
            TFile.Text = "";
            TPause.Visibility = TResume.Visibility = TCancel.Visibility = Visibility.Collapsed;
            TClose.Visibility = Visibility.Visible;
            Toast(text, status == "ok" ? "ok" : status == "error" ? "error" : "info");
            RenderAction();
        });
        _cancelRequested = false;
    }

    public Task<ReceiveDecision?> AskAcceptAsync(Offer offer, string fromIp) =>
        Dispatcher.InvokeAsync(() => AskUser(offer, $"{offer.From} ({fromIp})", incoming: true)).Task;

    ReceiveDecision? AskUser(Offer offer, string from, bool incoming)
    {
        var libs = Steam.GetLibraries();
        if (libs.Count == 0)
        {
            MessageBox.Show(this, "Steam isn't installed on this PC. Install Steam first, then try again.", Title, MessageBoxButton.OK, MessageBoxImage.Warning);
            return null;
        }
        if (WindowState == WindowState.Minimized) WindowState = WindowState.Normal;
        Activate();
        var dlg = new OfferDialog(offer, from, incoming, libs) { Owner = this };
        return dlg.ShowDialog() == true ? dlg.Decision : null;
    }

    public async Task AfterReceiveAsync(Offer offer, bool steamWasClosed)
    {
        Ui(LoadGames);
        if (steamWasClosed) { await Steam.RestartAsync(Log); Log($"{offer.Name} should now show as installed in Steam."); return; }
        if (!Steam.IsRunning) { Log($"Open Steam and {offer.Name} will show as installed."); return; }
        var restart = await Dispatcher.InvokeAsync(() => MessageBox.Show(this, $"{offer.Name} is copied. Steam needs a restart to see it.\n\nRestart Steam now?",
            Title, MessageBoxButton.YesNo, MessageBoxImage.Question) == MessageBoxResult.Yes);
        if (restart) await Steam.RestartAsync(Log);
    }

    void Ui(Action a)
    {
        if (Dispatcher.CheckAccess()) a();
        else Dispatcher.BeginInvoke(a);
    }
}
