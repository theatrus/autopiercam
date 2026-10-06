namespace AutoPierCam.Viewer;

// Two independently revisioned agent documents, not a pretend atomic save.
// Validate both before writing; stop on failure; never roll back an accepted
// write over concurrent edits. Each successful writer adopts its new baseline.
internal static class SettingsSaveBatch
{
    internal sealed record Result(bool Success, bool ImagingSaved, bool SharingSaved, string Message);

    internal static async Task<Result> RunAsync(bool imaging, bool sharing,
        Action validateImaging, Action validateSharing,
        Func<Task<bool>> saveImaging, Func<Task<bool>> saveSharing)
    {
        try
        {
            if (imaging) validateImaging();
            if (sharing) validateSharing();
        }
        catch (Exception error)
        {
            return new(false, false, false, "Nothing saved: " + error.Message);
        }
        if (imaging && !await saveImaging())
            return new(false, false, false, "Imaging save was not confirmed. Chatstronomy was not saved; review the feedback below.");
        if (sharing && !await saveSharing())
            return new(false, imaging, false, imaging
                ? "Imaging saved; Chatstronomy still needs attention. Its edits are retained."
                : "Chatstronomy save was not confirmed. Review the feedback below; edits are retained.");
        return new(true, imaging, sharing, "All settings saved.");
    }
}
