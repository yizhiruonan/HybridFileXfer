param([string]$action = "install")
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$startup = [Environment]::GetFolderPath('Startup')
$linkPath = Join-Path $startup 'HybridFileXfer-Receiver-cli.lnk'
$shell = New-Object -ComObject WScript.Shell
if ($action -eq "uninstall") {
    if (Test-Path $linkPath) { Remove-Item $linkPath -Force; Write-Host 'Autostart removed.' } else { Write-Host 'Autostart was not installed.' }
    exit 0
}
$exe = Join-Path $root 'HybridFileXfer-Receiver-cli.exe'
if (-not (Test-Path $exe)) {
    Write-Host 'Please run build.bat first.'
    exit 1
}
$vbs = Join-Path $root 'start-hidden.vbs'
$link = $shell.CreateShortcut($linkPath)
$link.TargetPath = Join-Path $env:SystemRoot 'System32\wscript.exe'
$link.Arguments = '"' + $vbs + '"'
$link.WorkingDirectory = $root
$link.Description = 'HybridFileXfer receiver (headless)'
$link.Save()
Write-Host 'Autostart installed:' $linkPath
Write-Host 'If you prefer the GUI, open HybridFileXfer-Receiver.exe and tick the autostart box.'
