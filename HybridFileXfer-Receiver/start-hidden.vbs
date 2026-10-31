' 无窗口后台运行命令行版接收器（用于开机自启）
Set shell = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
dir = fso.GetParentFolderName(WScript.ScriptFullName)
shell.CurrentDirectory = dir
shell.Run """" & dir & "\HybridFileXfer-Receiver-cli.exe""", 0, False
