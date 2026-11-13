using System.Diagnostics;
using System.Management;
using System.Net.NetworkInformation;
using Microsoft.Win32;

namespace SteamTransfer;

/// <summary>One-click per-user install: no admin needed except the single firewall prompt.</summary>
public static class Installer
{
    public const string AppName = "Steam Game Transfer";
    public const string Version = "1.1.0";
    const string UninstallKey = @"Software\Microsoft\Windows\CurrentVersion\Uninstall\SteamGameTransfer";

    public static string DataDir => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "SteamTransfer");
    public static string InstallDir => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Programs", "SteamTransfer");
    public static string InstalledExe => Path.Combine(InstallDir, "SteamTransfer.exe");
    public static string CurrentExe => Environment.ProcessPath!;
    public static bool IsRunningInstalled => string.Equals(Path.GetFullPath(CurrentExe), Path.GetFullPath(InstalledExe), StringComparison.OrdinalIgnoreCase);
    static string StartMenuLink => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Programs), AppName + ".lnk");
    static string DesktopLink => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.DesktopDirectory), AppName + ".lnk");

    /// <summary>Copies the exe into %LocalAppData%\Programs, adds shortcuts, an "Installed apps" entry and the firewall rule.</summary>
    public static void Install()
    {
        Directory.CreateDirectory(InstallDir);
        if (!IsRunningInstalled) File.Copy(CurrentExe, InstalledExe, overwrite: true);
        CreateShortcut(StartMenuLink, InstalledExe);
        CreateShortcut(DesktopLink, InstalledExe);

        using (var k = Registry.CurrentUser.CreateSubKey(UninstallKey))
        {
            k.SetValue("DisplayName", AppName);
            k.SetValue("DisplayVersion", Version);
            k.SetValue("Publisher", AppName);
            k.SetValue("DisplayIcon", InstalledExe);
            k.SetValue("InstallLocation", InstallDir);
            k.SetValue("UninstallString", $"\"{InstalledExe}\" --uninstall");
            k.SetValue("NoModify", 1, RegistryValueKind.DWord);
            k.SetValue("NoRepair", 1, RegistryValueKind.DWord);
            k.SetValue("EstimatedSize", (int)(new FileInfo(InstalledExe).Length / 1024), RegistryValueKind.DWord);
        }
        if (!Firewall.RuleExists()) Firewall.AddRules();
    }

    public static void LaunchInstalled() => Process.Start(new ProcessStartInfo(InstalledExe, "--after-install") { UseShellExecute = true });

    public static void Uninstall()
    {
        if (System.Windows.MessageBox.Show("Remove Steam Game Transfer from this PC?\nYour games are not touched.", AppName,
                System.Windows.MessageBoxButton.YesNo, System.Windows.MessageBoxImage.Question) != System.Windows.MessageBoxResult.Yes) return;
        foreach (var l in new[] { StartMenuLink, DesktopLink }) { try { File.Delete(l); } catch { } }
        try { Registry.CurrentUser.DeleteSubKeyTree(UninstallKey, false); } catch { }
        Firewall.RemoveRules();
        // The running exe can't delete itself; let cmd do it a moment after we exit.
        Process.Start(new ProcessStartInfo("cmd.exe",
            $"/c timeout /t 2 /nobreak >nul & rmdir /s /q \"{InstallDir}\" & rmdir /s /q \"{DataDir}\"")
        { CreateNoWindow = true, UseShellExecute = false });
        System.Windows.MessageBox.Show("Steam Game Transfer was removed.", AppName);
    }

    /// <summary>Puts a copy of the app on a drive so the other PC can run it straight from there.</summary>
    public static void CopyAppTo(string folder)
    {
        try
        {
            Directory.CreateDirectory(folder);
            var dest = Path.Combine(folder, "SteamTransfer.exe");
            if (string.Equals(Path.GetFullPath(dest), Path.GetFullPath(CurrentExe), StringComparison.OrdinalIgnoreCase)) return;
            if (File.Exists(dest) && new FileInfo(dest).Length == new FileInfo(CurrentExe).Length) return;
            File.Copy(CurrentExe, dest, overwrite: true);
        }
        catch { }
    }

    public static void CreateShortcut(string lnk, string target)
    {
        var t = Type.GetTypeFromProgID("WScript.Shell")!;
        dynamic shell = Activator.CreateInstance(t)!;
        dynamic s = shell.CreateShortcut(lnk);
        s.TargetPath = target;
        s.WorkingDirectory = Path.GetDirectoryName(target);
        s.Description = "Copy Steam games between PCs";
        s.IconLocation = target + ",0";
        s.Save();
    }
}

public sealed record LinkInfo(string Name, string Kind, bool Up, long Bps);
public sealed record DriveEntry(string Root, string Label, long Free, long Total, bool Removable, List<PackageInfo> Packages);
public sealed record PackageInfo(string Folder, string AppId, string Name, long Size);

public static class Hardware
{
    static bool? _usb4;

    /// <summary>Does this PC have a USB4 or Thunderbolt controller at all?</summary>
    public static bool HasUsb4Controller()
    {
        if (_usb4 != null) return _usb4.Value;
        try
        {
            using var s = new ManagementObjectSearcher("SELECT Name FROM Win32_PnPEntity WHERE Name LIKE '%USB4%' OR Name LIKE '%Thunderbolt%'");
            _usb4 = s.Get().Count > 0;
        }
        catch { _usb4 = false; }
        return _usb4.Value;
    }

    /// <summary>Physical network adapters, same view as Get-NetAdapter (no hidden filter drivers / phantom adapters).</summary>
    public static List<LinkInfo> Links()
    {
        try
        {
            var list = new List<LinkInfo>();
            using var s = new ManagementObjectSearcher(@"root\StandardCimv2",
                "SELECT Name, InterfaceDescription, MediaConnectState, Speed, HardwareInterface, NdisPhysicalMedium FROM MSFT_NetAdapter WHERE Hidden = FALSE");
            foreach (ManagementObject o in s.Get())
            {
                var name = o["Name"] as string ?? "";
                var desc = o["InterfaceDescription"] as string ?? "";
                bool tb = desc.Contains("Thunderbolt", StringComparison.OrdinalIgnoreCase) || desc.Contains("USB4", StringComparison.OrdinalIgnoreCase);
                bool hw = o["HardwareInterface"] is true;
                if (!(hw || tb) || desc.Contains("Bluetooth", StringComparison.OrdinalIgnoreCase)) continue;
                bool up = Convert.ToUInt32(o["MediaConnectState"] ?? 0u) == 1;
                var medium = Convert.ToUInt32(o["NdisPhysicalMedium"] ?? 0u);
                var kind = tb ? "usb4" : medium is 1 or 9 ? "wifi" : "ethernet"; // 1 = WirelessLan, 9 = Native802_11
                list.Add(new LinkInfo(name, kind, up, up ? (long)Convert.ToUInt64(o["Speed"] ?? 0ul) : 0));
            }
            return list;
        }
        catch { return LinksFallback(); }
    }

    static List<LinkInfo> LinksFallback() =>
        NetworkInterface.GetAllNetworkInterfaces()
            .Where(n => n.NetworkInterfaceType is NetworkInterfaceType.Ethernet or NetworkInterfaceType.GigabitEthernet
                        or NetworkInterfaceType.Wireless80211 or NetworkInterfaceType.FastEthernetT || Discovery.IsThunderbolt(n))
            .Where(n => !n.Description.Contains("Virtual", StringComparison.OrdinalIgnoreCase)
                        && !n.Description.Contains("Hyper-V", StringComparison.OrdinalIgnoreCase)
                        && !n.Description.Contains("Bluetooth", StringComparison.OrdinalIgnoreCase)
                        && !n.Description.Contains("Kernel Debug", StringComparison.OrdinalIgnoreCase)
                        && !n.Description.Contains("WAN Miniport", StringComparison.OrdinalIgnoreCase))
            .Select(n => new LinkInfo(n.Name, Discovery.KindOf(n), n.OperationalStatus == OperationalStatus.Up, n.OperationalStatus == OperationalStatus.Up ? n.Speed : 0))
            .ToList();

    /// <summary>Drives you could copy a game to: removable drives and any non-system drive (external SSDs report as fixed).</summary>
    public static List<DriveEntry> Drives()
    {
        var sys = Path.GetPathRoot(Environment.SystemDirectory);
        var physical = PhysicalLetters();
        var list = new List<DriveEntry>();
        foreach (var d in DriveInfo.GetDrives())
        {
            try
            {
                if (d.DriveType is not (DriveType.Removable or DriveType.Fixed) || !d.IsReady) continue;
                if (string.Equals(d.Name, sys, StringComparison.OrdinalIgnoreCase)) continue;
                if (physical != null && !physical.Contains(d.Name[..2])) continue; // cloud/virtual drives (Google Drive etc.)
                if (d.TotalSize < 2L * 1024 * 1024 * 1024) continue;              // tiny system/recovery partitions
                list.Add(new DriveEntry(d.Name, string.IsNullOrWhiteSpace(d.VolumeLabel) ? (d.DriveType == DriveType.Removable ? "USB drive" : "Local disk") : d.VolumeLabel,
                    d.AvailableFreeSpace, d.TotalSize, d.DriveType == DriveType.Removable, FindPackages(d.Name)));
            }
            catch { }
        }
        return list;
    }

    /// <summary>Drive letters that sit on a real disk partition (null if WMI is unavailable).</summary>
    static HashSet<string>? PhysicalLetters()
    {
        try
        {
            var set = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            using var s = new ManagementObjectSearcher("SELECT Dependent FROM Win32_LogicalDiskToPartition");
            foreach (ManagementObject o in s.Get())
            {
                var dep = o["Dependent"] as string ?? ""; // ...Win32_LogicalDisk.DeviceID="E:"
                var i = dep.IndexOf("DeviceID=\"", StringComparison.Ordinal);
                if (i >= 0) set.Add(dep.Substring(i + 10, 2));
            }
            return set;
        }
        catch { return null; }
    }

    public static List<PackageInfo> FindPackages(string folder)
    {
        var list = new List<PackageInfo>();
        var candidates = new List<string> { folder };
        try
        {
            candidates.AddRange(Directory.EnumerateDirectories(folder));
            var st = Path.Combine(folder, "SteamTransfer");
            if (Directory.Exists(st)) candidates.AddRange(Directory.EnumerateDirectories(st));
        }
        catch { }
        foreach (var dir in candidates.Distinct(StringComparer.OrdinalIgnoreCase))
        {
            var file = Path.Combine(dir, Transfer.PackageFile);
            if (!File.Exists(file)) continue;
            try
            {
                var o = System.Text.Json.JsonSerializer.Deserialize<Offer>(File.ReadAllText(file));
                if (o != null) list.Add(new PackageInfo(dir, o.AppId, o.Name, o.TotalBytes));
            }
            catch { }
        }
        return list;
    }
}
