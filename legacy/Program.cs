using System.Windows;

namespace SteamTransfer;

static class Program
{
    [STAThread]
    static void Main(string[] args)
    {
        if (args.Contains("--uninstall", StringComparer.OrdinalIgnoreCase)) { Installer.Uninstall(); return; }

        // Only one copy at a time (they would fight over the network ports).
        using var mutex = new Mutex(true, @"Local\SteamGameTransfer", out var first);
        if (!first && args.Contains("--after-install"))
        {
            // The installer copy is still closing; wait for it.
            try { first = mutex.WaitOne(TimeSpan.FromSeconds(15)); }
            catch (AbandonedMutexException) { first = true; }
        }
        if (!first)
        {
            MessageBox.Show("Steam Game Transfer is already open.", Installer.AppName, MessageBoxButton.OK, MessageBoxImage.Information);
            return;
        }

        var app = new Application
        {
            ShutdownMode = ShutdownMode.OnMainWindowClose,
            // Native Windows 11 look: Fluent controls, system accent colour, light/dark from Windows settings, Mica backdrop.
            ThemeMode = ThemeMode.System,
        };
        app.DispatcherUnhandledException += (_, e) =>
        {
            MessageBox.Show(e.Exception.Message, Installer.AppName, MessageBoxButton.OK, MessageBoxImage.Error);
            e.Handled = true;
        };
        app.Run(new MainWindow());
    }
}
