using Microsoft.UI.Xaml;
using Windows.Graphics;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow : Window
{
    public MainWindow()
    {
        InitializeComponent();
        Title = "AutoPierCam";
        AppWindow.Resize(new SizeInt32(1180, 760));
    }

    private void RefreshButton_Click(object sender, RoutedEventArgs e)
    {
        StatusText.Text = "Capture agent is not running yet";
    }

    private void CaptureButton_Click(object sender, RoutedEventArgs e)
    {
        StatusText.Text = "Capture request requires the local agent IPC";
    }

    private void SaveButton_Click(object sender, RoutedEventArgs e)
    {
        StatusText.Text = "Settings were not changed; local agent IPC is not connected";
    }
}

