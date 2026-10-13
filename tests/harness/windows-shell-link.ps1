#requires -Version 7.2
# WScript.Shell can convert shortcut strings through an ANSI code page. Use the
# explicit wide Shell Link interface for independent acceptance readback.
if (-not ('AvelinAcceptance.ShellLink' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
using System.Text;

namespace AvelinAcceptance
{
    public sealed class ShellLinkValues
    {
        public string TargetPath { get; internal set; }
        public string Arguments { get; internal set; }
        public string WorkingDirectory { get; internal set; }
        public string Description { get; internal set; }
    }

    [ComImport, Guid("000214F9-0000-0000-C000-000000000046"),
        InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    internal interface IShellLinkW
    {
        void GetPath([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder path,
            int capacity, IntPtr findData, uint flags);
        void GetIDList(out IntPtr itemIdList);
        void SetIDList(IntPtr itemIdList);
        void GetDescription([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder description, int capacity);
        void SetDescription([MarshalAs(UnmanagedType.LPWStr)] string description);
        void GetWorkingDirectory([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder directory, int capacity);
        void SetWorkingDirectory([MarshalAs(UnmanagedType.LPWStr)] string directory);
        void GetArguments([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder arguments, int capacity);
        void SetArguments([MarshalAs(UnmanagedType.LPWStr)] string arguments);
        void GetHotkey(out short hotkey);
        void SetHotkey(short hotkey);
        void GetShowCmd(out int showCommand);
        void SetShowCmd(int showCommand);
        void GetIconLocation([Out, MarshalAs(UnmanagedType.LPWStr)] StringBuilder iconPath, int capacity, out int index);
        void SetIconLocation([MarshalAs(UnmanagedType.LPWStr)] string iconPath, int index);
        void SetRelativePath([MarshalAs(UnmanagedType.LPWStr)] string path, uint reserved);
        void Resolve(IntPtr window, uint flags);
        void SetPath([MarshalAs(UnmanagedType.LPWStr)] string path);
    }

    public static class ShellLink
    {
        private const int Capacity = 32768;
        private static readonly Guid ClassId = new Guid("00021401-0000-0000-C000-000000000046");

        private static string ExistingFile(string path)
        {
            if (String.IsNullOrWhiteSpace(path)) throw new ArgumentException("Shortcut path is empty.", nameof(path));
            string fullPath = Path.GetFullPath(path);
            if (!File.Exists(fullPath)) throw new FileNotFoundException("Shortcut file does not exist.", fullPath);
            if ((File.GetAttributes(fullPath) & (FileAttributes.Directory | FileAttributes.ReparsePoint)) != 0)
                throw new IOException("Shortcut must be an existing regular file.");
            return fullPath;
        }

        public static ShellLinkValues Read(string path)
        {
            string fullPath = ExistingFile(path);
            object instance = null;
            try
            {
                instance = Activator.CreateInstance(Type.GetTypeFromCLSID(ClassId, true));
                ((IPersistFile)instance).Load(fullPath, 0); // STGM_READ; never resolve or execute the target.
                var link = (IShellLinkW)instance;
                var target = new StringBuilder(Capacity);
                var arguments = new StringBuilder(Capacity);
                var workingDirectory = new StringBuilder(Capacity);
                var description = new StringBuilder(Capacity);
                link.GetPath(target, Capacity, IntPtr.Zero, 0); // Default actual path, not SLGP_RAWPATH.
                link.GetArguments(arguments, Capacity);
                link.GetWorkingDirectory(workingDirectory, Capacity);
                link.GetDescription(description, Capacity);
                return new ShellLinkValues {
                    TargetPath = target.ToString(), Arguments = arguments.ToString(),
                    WorkingDirectory = workingDirectory.ToString(), Description = description.ToString()
                };
            }
            finally
            {
                if (instance != null && Marshal.IsComObject(instance)) Marshal.FinalReleaseComObject(instance);
            }
        }

        public static void SetDescription(string path, string description)
        {
            string fullPath = ExistingFile(path);
            if (description == null) throw new ArgumentNullException(nameof(description));
            object instance = null;
            try
            {
                instance = Activator.CreateInstance(Type.GetTypeFromCLSID(ClassId, true));
                var persist = (IPersistFile)instance;
                persist.Load(fullPath, 2); // STGM_READWRITE on this already-existing shortcut only.
                ((IShellLinkW)instance).SetDescription(description);
                persist.Save(fullPath, true);
            }
            finally
            {
                if (instance != null && Marshal.IsComObject(instance)) Marshal.FinalReleaseComObject(instance);
            }
        }
    }
}
'@
}
