# Installs PingPath with its Windows installer, installs it again over the
# running service, uninstalls it, and checks that nothing is left behind
# (PLAN.md Phase 3: "Uninstall leaves no routes, adapters or services
# behind"). CI runs it on a fresh windows-latest runner, whose jobs run as
# an administrator, with the installer the installer job built:
#
#   pwsh -File tools/check-installer.ps1 -Installer <PingPath_..._x64-setup.exe> -WintunSha256 <hex> [-Diagnostics <folder>]
#
# It changes the machine it runs on, so run it only on a throwaway one.
#
# 1. Snapshot: every route (IPv4 and IPv6, active and persistent stores),
#    network adapter (hidden ones too), network device (non-present ones
#    too), service and driver. Nothing of PingPath's may be there yet.
# 2. Install silently and check: exit code 0; the files are in place,
#    wintun.dll among them with its pinned SHA-256; the PingPath service
#    is registered to start automatically, from the install folder, and
#    runs; its named pipe is up; %ProgramData%\PingPath exists.
# 3. Leave what a crashed service leaves: a relay host route (metric 293,
#    through the runner's gateway) recorded in the service's route ledger.
#    The routes go to TEST-NET-1 addresses (192.0.2.0/24), which carry no
#    real traffic. A service that never ran a session has no routes, so
#    without this the steps below would have nothing to remove. Nothing
#    here creates the adapter: that takes a session through real relays.
#    client/core/tests/windows_backend.rs kills a process holding the
#    adapter and checks the cleanup the service and `uninstall` run.
# 4. Install again, over the running service, as a silent install over an
#    existing one does (and an upgrade when the player keeps the old
#    version): the installer's hooks must stop the service before
#    replacing its files and start it again (a new process), and the
#    service, starting, must remove the route from step 3 and its ledger
#    record. Same checks as step 2.
# 5. Leave another such route, then uninstall silently and check: exit
#    code 0; the service, the adapter, every route on a PingPath interface
#    or with metric 293, %ProgramData%\PingPath, the install folder, the
#    shortcuts and the uninstaller's registry key are gone; routes,
#    adapters and network devices match the snapshot exactly; and no
#    service or driver of PingPath's or Wintun's was added. Other services
#    that came or went meanwhile (the WebView2 runtime's updater, which
#    the installer adds when the runner lacks the runtime, or the runner's
#    own updaters) are listed as warnings: they aren't PingPath's.
#
# Installers run with Start-Process -PassThru and a time limit rather than
# Start-Process -Wait: PowerShell 7 waits for the process's whole tree,
# and the WebView2 bootstrapper the installer may run leaves Edge's updater
# running, which would hang the job. The exit code is the installer's own.
#
# Every failure says what was expected and what was found, and prints the
# service's own log and what Windows recorded about it (PLAN.md section 9).
# A silent installer keeps the output of the service commands its hooks
# run to itself, so when the installer or the uninstaller fails, those
# commands are run again at the end of the diagnostics to show their
# errors. -Diagnostics gets the snapshots and a copy of the service's logs,
# which uninstalling deletes; CI keeps that folder when the check fails.

[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Installer,
    [Parameter(Mandatory)] [string] $WintunSha256,
    [string] $Diagnostics = (Join-Path ([IO.Path]::GetTempPath()) 'pingpath-installer-check')
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

# Names the installer and the service use: client/ui/tauri.conf.json,
# client/service/src/windows/scm.rs and paths.rs, client/core/src/ipc.rs,
# ADAPTER_NAME in client/core/src/net/mod.rs, ADAPTER_GUID in
# client/core/src/net/windows/adapter.rs and HOST_ROUTE_METRIC in
# client/core/src/net/route_table.rs.
$ServiceName = 'PingPath'
$Pipe = '\\.\pipe\pingpath'
$AdapterGuid = '066C8B67-BABF-4B6D-8AC5-666510895AAB'
$HostRouteMetric = 293
$UninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PingPath'
$DataDir = Join-Path $env:ProgramData 'PingPath'
$Ledger = Join-Path $DataDir 'state\routes.json'
# Step 3's route, which the restarted service removes, and step 5's,
# which uninstalling removes.
$LeftoverRoutes = @('192.0.2.1/32', '192.0.2.2/32')
# The WebView2 runtime's registration, which the installer checks to
# decide whether to install the runtime.
$WebView2Key = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
# Long enough for the installer to download and install WebView2 when the
# runner lacks it.
$SetupTimeoutSeconds = 600
# How long the service may take to start, stop, or clean up as it starts.
$ServiceTimeoutSeconds = 30

# When the check started, for the event log.
$script:Started = Get-Date
# The service commands the installer's hooks ran, to run again if the
# installer or uninstaller fails: Exe is $null for the installed copy.
$script:Rerun = $null

function Write-Step([string] $Text) {
    Write-Host ''
    Write-Host "== $Text"
}

# Polls $Condition every half second until it holds or $Seconds pass.
function Wait-Until([int] $Seconds, [scriptblock] $Condition) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while (-not (& $Condition)) {
        if ((Get-Date) -ge $deadline) {
            return $false
        }
        Start-Sleep -Milliseconds 500
    }
    return $true
}

function Assert-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "this check runs installers and changes routes, so it needs an elevated administrator, and $($identity.Name) isn't one here"
    }
}

# ---- What the machine has ---------------------------------------------------

# Routes as one line each. No -AddressFamily, so IPv4 and IPv6 both; the
# active store is what Windows routes by, the persistent store what it
# restores at boot. The persistent store is often empty, which
# Get-NetRoute reports as an error.
function Get-RouteList {
    $lines = foreach ($store in 'ActiveStore', 'PersistentStore') {
        $routes = if ($store -eq 'ActiveStore') {
            Get-NetRoute -PolicyStore $store
        } else {
            Get-NetRoute -PolicyStore $store -ErrorAction SilentlyContinue
        }
        foreach ($route in @($routes)) {
            '{0}: {1} via {2} on {3} (index {4}) metric {5}' -f $store, $route.DestinationPrefix,
                $route.NextHop, $route.InterfaceAlias, $route.InterfaceIndex, $route.RouteMetric
        }
    }
    return @($lines | Sort-Object -Unique)
}

function Get-AdapterList {
    return @(Get-NetAdapter -IncludeHidden | ForEach-Object {
            '{0} | {1} | {2}' -f $_.Name, $_.InterfaceDescription, $_.InterfaceGuid
        } | Sort-Object -Unique)
}

# Non-present devices too: a removed adapter can leave its device behind.
# Get-PnpDevice returns Win32_PnPEntity objects: DeviceID is the instance ID
# (a Wintun adapter's is SWD\WINTUN\{its GUID}).
function Get-NetworkDeviceList {
    return @(Get-PnpDevice -Class Net | ForEach-Object {
            '{0} | {1}' -f $_.DeviceID, $_.Name
        } | Sort-Object -Unique)
}

# Through WMI, which lists kernel drivers (Wintun's would be one) as well as
# services, with the program each runs. Get-Service would also keep a
# handle to each service it returns, and a service deleted while a handle
# is open stays "marked for deletion" until the handle closes.
function Get-ServiceList {
    return @(Get-CimInstance -ClassName Win32_Service | ForEach-Object {
            '{0} | {1}' -f $_.Name, $_.PathName
        } | Sort-Object -Unique)
}

function Get-DriverList {
    return @(Get-CimInstance -ClassName Win32_SystemDriver | ForEach-Object {
            '{0} | {1}' -f $_.Name, $_.PathName
        } | Sort-Object -Unique)
}

function Get-Snapshot {
    return [ordered]@{
        Routes = @(Get-RouteList)
        Adapters = @(Get-AdapterList)
        NetworkDevices = @(Get-NetworkDeviceList)
        Services = @(Get-ServiceList)
        Drivers = @(Get-DriverList)
    }
}

function Save-Snapshot($Snapshot, [string] $Name) {
    foreach ($key in $Snapshot.Keys) {
        $path = Join-Path $Diagnostics "$Name-$key.txt"
        [IO.File]::WriteAllLines($path, [string[]] @($Snapshot[$key]))
    }
}

# What is in $After and not in $Before ("added") and the other way round.
function Compare-List([string[]] $Before, [string[]] $After) {
    return [pscustomobject]@{
        Added = @(@($After) | Where-Object { $Before -notcontains $_ })
        Removed = @(@($Before) | Where-Object { $After -notcontains $_ })
    }
}

function Get-PingPathService {
    return Get-CimInstance -ClassName Win32_Service -Filter "Name='$ServiceName'"
}

function Get-PingPathRoute {
    return @(foreach ($store in 'ActiveStore', 'PersistentStore') {
            Get-NetRoute -PolicyStore $store -ErrorAction SilentlyContinue | Where-Object {
                $_.RouteMetric -eq $HostRouteMetric -or $_.InterfaceAlias -like 'PingPath*' -or
                $LeftoverRoutes -contains $_.DestinationPrefix
            }
        })
}

function Get-PingPathAdapter {
    return @(Get-NetAdapter -IncludeHidden | Where-Object {
            $_.InterfaceGuid -eq "{$AdapterGuid}" -or $_.Name -like 'PingPath*' -or
            $_.InterfaceDescription -like 'PingPath*'
        })
}

function Get-PingPathDevice {
    return @(Get-PnpDevice -Class Net | Where-Object {
            $_.DeviceID -like "*$AdapterGuid*" -or $_.Name -like 'PingPath*'
        })
}

# The Start menu and desktop shortcuts, for all users (a per-machine
# install) and for this one.
function Get-PingPathShortcut {
    $folders = @('CommonDesktopDirectory', 'CommonPrograms', 'Desktop', 'Programs' | ForEach-Object {
            [Environment]::GetFolderPath($_)
        } | Where-Object { $_ } | Sort-Object -Unique)
    return @(foreach ($folder in $folders) {
            Get-ChildItem -LiteralPath $folder -Filter 'PingPath*' -Recurse -Force -ErrorAction SilentlyContinue
        })
}

function Get-InstallDir {
    if (-not (Test-Path -LiteralPath $UninstallKey)) {
        throw "the installer registered no uninstaller: $UninstallKey is missing"
    }
    # The installer writes the folder in quotes.
    return (Get-ItemPropertyValue -LiteralPath $UninstallKey -Name InstallLocation).Trim('"')
}

function Read-Ledger {
    try {
        if (Test-Path -LiteralPath $Ledger) {
            return (Get-Content -LiteralPath $Ledger -Raw)
        }
        return "(no $Ledger)"
    } catch {
        return "(cannot read $Ledger`: $($_.Exception.Message))"
    }
}

# ---- Diagnostics --------------------------------------------------------------

function Show-ServiceLog {
    $logs = Join-Path $DataDir 'logs'
    if (-not (Test-Path -LiteralPath $logs)) {
        Write-Host "(no service logs: $logs doesn't exist)"
        return
    }
    foreach ($file in @(Get-ChildItem -LiteralPath $logs -File)) {
        Write-Host "-- last lines of $($file.FullName)"
        Get-Content -LiteralPath $file.FullName -Tail 40 | ForEach-Object { Write-Host $_ }
    }
}

function Save-ServiceLog {
    $logs = Join-Path $DataDir 'logs'
    if (Test-Path -LiteralPath $logs) {
        Copy-Item -LiteralPath $logs -Destination (Join-Path $Diagnostics 'service-logs') -Recurse -Force
    }
}

# What Windows recorded about the service since the check started: the
# Service Control Manager's events (installed, started, stopped, failed to
# start, terminated unexpectedly) and crash reports.
function Show-WindowsEvent {
    $sources = @(
        @{ LogName = 'System'; ProviderName = 'Service Control Manager' },
        @{ LogName = 'Application'; ProviderName = 'Application Error' },
        @{ LogName = 'Application'; ProviderName = 'Windows Error Reporting' }
    )
    foreach ($source in $sources) {
        Write-Host "-- $($source.LogName) log, $($source.ProviderName): events about PingPath"
        $filter = $source + @{ StartTime = $script:Started }
        $records = @(Get-WinEvent -FilterHashtable $filter -ErrorAction SilentlyContinue |
                Where-Object { $_.Message -match 'pingpath' } | Sort-Object TimeCreated)
        if ($records.Count -eq 0) {
            Write-Host '(none)'
        }
        foreach ($record in $records) {
            Write-Host ('{0:HH:mm:ss} event {1}: {2}' -f $record.TimeCreated, $record.Id, ($record.Message -replace '\s+', ' '))
        }
    }
}

# Runs again the service commands a failed installer or uninstaller ran,
# whose output a silent run keeps to itself.
function Show-HookCommand {
    if ($null -eq $script:Rerun) {
        return
    }
    $exe = $script:Rerun.Exe
    if ($null -eq $exe) {
        $dir = try { Get-InstallDir } catch { Join-Path $env:ProgramFiles 'PingPath' }
        $exe = Join-Path $dir 'pingpath-service.exe'
    }
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        Write-Host "(can't run the hooks' service commands again to show their errors: $exe is missing)"
        return
    }
    foreach ($command in $script:Rerun.Commands) {
        Write-Host "-- run again, as the installer's hooks ran it: `"$exe`" $command"
        $output = & $exe $command 2>&1
        $code = $LASTEXITCODE
        foreach ($line in @($output)) {
            Write-Host "   $line"
        }
        Write-Host "   exit code $code"
    }
}

# Everything that could explain a failure. Never throws: it runs when
# something already has.
function Show-Diagnostics {
    $ErrorActionPreference = 'Continue'
    Write-Step 'Diagnostics'
    try {
        Write-Host "-- sc.exe query / qc / qfailure $ServiceName"
        sc.exe query $ServiceName | Write-Host
        sc.exe qc $ServiceName | Write-Host
        sc.exe qfailure $ServiceName | Write-Host
        Show-WindowsEvent
        Write-Host "-- what of PingPath's is on the machine now"
        $dir = try { Get-InstallDir } catch { Join-Path $env:ProgramFiles 'PingPath' }
        $found = @(Find-PingPath $dir)
        if ($found.Count -eq 0) {
            Write-Host '(nothing)'
        }
        foreach ($line in $found) {
            Write-Host $line
        }
        Write-Host "-- the route ledger, $Ledger"
        Write-Host (Read-Ledger)
        Write-Host "-- $UninstallKey"
        Get-ItemProperty -LiteralPath $UninstallKey -ErrorAction SilentlyContinue | Format-List | Out-String -Width 300 | Write-Host
        Write-Host '-- what the PingPath folders hold'
        Get-ChildItem -Path (Join-Path $env:ProgramData 'PingPath*'), (Join-Path $env:ProgramFiles 'PingPath*') -Recurse -Force -ErrorAction SilentlyContinue |
            ForEach-Object { Write-Host $_.FullName }
        Show-ServiceLog
        $saved = Join-Path $Diagnostics 'service-logs'
        if (Test-Path -LiteralPath $saved) {
            foreach ($file in @(Get-ChildItem -LiteralPath $saved -File)) {
                Write-Host "-- last lines of the service log saved before uninstalling: $($file.Name)"
                Get-Content -LiteralPath $file.FullName -Tail 40 | ForEach-Object { Write-Host $_ }
            }
        }
        Save-Snapshot (Get-Snapshot) 'failure'
        # Last: running them again changes what is above.
        Show-HookCommand
        Write-Host "Snapshots and saved logs: $Diagnostics"
    } catch {
        Write-Host "(diagnostics stopped: $($_.Exception.Message))"
    }
}

# ---- Installing and uninstalling --------------------------------------------

# Runs an NSIS installer or uninstaller and fails on any exit code but 0.
# It waits for that process only, with a time limit (see the top).
function Invoke-Setup([string] $FilePath, [string] $Arguments, [string] $What) {
    Write-Host "> `"$FilePath`" $Arguments"
    $process = Start-Process -FilePath $FilePath -ArgumentList $Arguments -PassThru
    # Holding the handle keeps the exit code readable after the process ends.
    $null = $process.Handle
    $clock = [Diagnostics.Stopwatch]::StartNew()
    if (-not $process.WaitForExit($SetupTimeoutSeconds * 1000)) {
        $process.Kill($true)
        throw "$What didn't finish within $SetupTimeoutSeconds s"
    }
    $code = $process.ExitCode
    Write-Host ('{0} exited with code {1} after {2:N1} s' -f $What, $code, $clock.Elapsed.TotalSeconds)
    if ($code -ne 0) {
        # NSIS: 1 is cancelled, 2 aborted. PingPath's hooks abort with 2
        # when the service can't be stopped, installed or removed.
        throw "$What failed with exit code $code"
    }
}

function Invoke-Installer([string] $Setup, [string] $What, [string[]] $HookCommands) {
    $script:Rerun = @{ Exe = $null; Commands = $HookCommands }
    Invoke-Setup -FilePath $Setup -Arguments '/S' -What $What
    $script:Rerun = $null
}

# Checks what installing must leave, and returns the install folder and
# the service's process ID.
function Assert-Installed([string] $When) {
    $dir = Get-InstallDir
    Write-Host "installed in $dir"
    foreach ($file in 'PingPath.exe', 'pingpath-service.exe', 'wintun.dll', 'uninstall.exe') {
        if (-not (Test-Path -LiteralPath (Join-Path $dir $file) -PathType Leaf)) {
            throw "${When}: $file is missing from $dir"
        }
    }
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $dir 'wintun.dll')).Hash.ToLowerInvariant()
    if ($hash -ne $WintunSha256.ToLowerInvariant()) {
        throw "${When}: the installed wintun.dll has SHA-256 $hash, not the pinned $WintunSha256"
    }

    $running = Wait-Until $ServiceTimeoutSeconds {
        $service = Get-PingPathService
        $null -ne $service -and $service.State -eq 'Running'
    }
    $service = Get-PingPathService
    if ($null -eq $service) {
        throw "${When}: no service named $ServiceName is registered"
    }
    Write-Host ('service {0}: {1} (process {2}), start mode {3}, command line {4}' -f $service.Name,
        $service.State, $service.ProcessId, $service.StartMode, $service.PathName)
    if (-not $running) {
        throw "${When}: the $ServiceName service is $($service.State) $ServiceTimeoutSeconds s after installing, not Running"
    }
    if ($service.StartMode -ne 'Auto') {
        throw "${When}: the $ServiceName service starts $($service.StartMode), not Auto"
    }
    $exe = Join-Path $dir 'pingpath-service.exe'
    if ($service.PathName.IndexOf($exe, [StringComparison]::OrdinalIgnoreCase) -lt 0) {
        throw "${When}: the $ServiceName service runs $($service.PathName), not $exe"
    }
    # Listing the pipes doesn't connect to any: the service sees nothing.
    $serving = Wait-Until $ServiceTimeoutSeconds { [IO.Directory]::GetFiles('\\.\pipe\') -contains $Pipe }
    if (-not $serving) {
        throw "${When}: the service runs but $Pipe doesn't exist $ServiceTimeoutSeconds s after installing"
    }
    Write-Host "$Pipe is up"
    if (-not (Test-Path -LiteralPath $DataDir -PathType Container)) {
        throw "${When}: the service runs but $DataDir doesn't exist"
    }
    return [pscustomobject]@{ Dir = $dir; ProcessId = $service.ProcessId }
}

# A relay host route and its ledger record, as a crashed service leaves
# them (client/core/src/net/route_table.rs: RouteRow, Ledger): the ledger
# names the route exactly, so the service, starting, and uninstalling
# must remove it. The running service read its ledger when it started and
# writes it only when it adds or removes a route of its own, so this file
# is what the next process to start reads.
function Add-LeftoverRoute([string] $Prefix) {
    $defaults = @(Get-NetRoute -AddressFamily IPv4 -PolicyStore ActiveStore |
            Where-Object DestinationPrefix -eq '0.0.0.0/0' | Sort-Object RouteMetric)
    if ($defaults.Count -eq 0) {
        throw 'the runner has no IPv4 default route to leave a relay host route on'
    }
    $via = $defaults[0]
    if ($via.NextHop -eq '0.0.0.0') {
        throw "the runner's default route has no gateway (interface $($via.InterfaceAlias))"
    }
    $adapters = @(Get-NetAdapter -IncludeHidden | Where-Object InterfaceIndex -eq $via.InterfaceIndex)
    if ($adapters.Count -ne 1) {
        throw "found $($adapters.Count) adapters with the default route's interface index $($via.InterfaceIndex)"
    }
    New-NetRoute -DestinationPrefix $Prefix -InterfaceIndex $via.InterfaceIndex -NextHop $via.NextHop `
        -RouteMetric $HostRouteMetric -PolicyStore ActiveStore | Out-Null
    $record = [ordered]@{
        routes = @(
            [ordered]@{
                interface = [uint64] $adapters[0].NetLuid
                destination = $Prefix
                next_hop = $via.NextHop
                metric = $HostRouteMetric
            }
        )
    } | ConvertTo-Json -Depth 4
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Ledger) | Out-Null
    # UTF-8 without a byte order mark, as the service writes it.
    [IO.File]::WriteAllText($Ledger, $record)
    Write-Host "route to $Prefix via $($via.NextHop) on $($via.InterfaceAlias), metric $HostRouteMetric; ${Ledger}:"
    Write-Host $record
    $added = @(Get-NetRoute -PolicyStore ActiveStore | Where-Object {
            $_.DestinationPrefix -eq $Prefix -and $_.RouteMetric -eq $HostRouteMetric
        })
    if ($added.Count -ne 1) {
        throw "the leftover route to $Prefix isn't in the route table after adding it"
    }
}

# The service removes ledger routes as it starts (the engine's cleanup),
# and records that in the ledger.
function Assert-LeftoverRemoved([string] $Prefix) {
    $inTable = { @(Get-NetRoute -DestinationPrefix $Prefix -PolicyStore ActiveStore -ErrorAction SilentlyContinue).Count -gt 0 }
    $inLedger = { (Read-Ledger) -match [regex]::Escape("`"$Prefix`"") }
    $done = Wait-Until $ServiceTimeoutSeconds { -not (& $inTable) -and -not (& $inLedger) }
    if (-not $done) {
        $where = @(if (& $inTable) { 'in the route table' }; if (& $inLedger) { 'in the ledger' }) -join ' and '
        throw "the service, started again, didn't remove the relay host route to $Prefix that its ledger lists: still $where $ServiceTimeoutSeconds s later. Ledger: $(Read-Ledger)"
    }
    Write-Host "the service, started again, removed the route to $Prefix and its ledger record"
}

# An NSIS uninstaller copies itself to %TEMP%, starts the copy and exits at
# once, so its exit code says nothing. Run as `uninstall.exe /S _?=<folder>`
# it uninstalls in place and returns its own exit code, but it can't delete
# itself, so the folder would stay. A copy run from elsewhere with _?= does
# both, as NSIS's own hand-off does. _?= must come last, unquoted.
function Invoke-Uninstaller([string] $InstallDir) {
    $copyDir = Join-Path ([IO.Path]::GetTempPath()) "pingpath-uninstall-$([guid]::NewGuid())"
    New-Item -ItemType Directory -Path $copyDir | Out-Null
    # The service and wintun.dll too, to run `uninstall` again should the
    # uninstaller fail: by then it has deleted them.
    Copy-Item -LiteralPath (Join-Path $InstallDir 'uninstall.exe'), (Join-Path $InstallDir 'pingpath-service.exe'),
        (Join-Path $InstallDir 'wintun.dll') -Destination $copyDir
    $script:Rerun = @{ Exe = (Join-Path $copyDir 'pingpath-service.exe'); Commands = @('uninstall') }
    Invoke-Setup -FilePath (Join-Path $copyDir 'uninstall.exe') -Arguments "/S _?=$InstallDir" -What 'the uninstaller'
    $script:Rerun = $null
    Remove-Item -LiteralPath $copyDir -Recurse -Force -ErrorAction SilentlyContinue
}

# Anything of PingPath's on the machine, one line each: before installing
# there must be none, after uninstalling too.
function Find-PingPath([string] $InstallDir) {
    $found = [Collections.Generic.List[string]]::new()
    $service = Get-PingPathService
    if ($null -ne $service) {
        $found.Add("the $ServiceName service ($($service.State), $($service.PathName))")
    }
    foreach ($adapter in Get-PingPathAdapter) {
        $found.Add("an adapter: $($adapter.Name) ($($adapter.InterfaceDescription), $($adapter.InterfaceGuid))")
    }
    foreach ($device in Get-PingPathDevice) {
        $found.Add("a network device: $($device.DeviceID) ($($device.Name), $($device.Status))")
    }
    foreach ($route in Get-PingPathRoute) {
        $what = if ($LeftoverRoutes -contains $route.DestinationPrefix) {
            "the relay host route left as a crash would, which the service's ledger names"
        } else {
            "a route on a PingPath interface or with metric $HostRouteMetric"
        }
        $found.Add(('{0}: {1} via {2} on {3}, metric {4}' -f $what, $route.DestinationPrefix,
                $route.NextHop, $route.InterfaceAlias, $route.RouteMetric))
    }
    foreach ($folder in @(Get-ChildItem -LiteralPath $env:ProgramData -Filter 'PingPath*' -Force)) {
        $found.Add("a data folder: $($folder.FullName)")
    }
    if ($InstallDir -and (Test-Path -LiteralPath $InstallDir)) {
        $files = @(Get-ChildItem -LiteralPath $InstallDir -Recurse -Force | ForEach-Object Name) -join ', '
        $found.Add("the install folder: $InstallDir (holding: $files)")
    }
    foreach ($shortcut in Get-PingPathShortcut) {
        $found.Add("a shortcut: $($shortcut.FullName)")
    }
    if (Test-Path -LiteralPath $UninstallKey) {
        $found.Add("the uninstaller's registry key: $UninstallKey")
    }
    return @($found)
}

# Everything uninstalling must have removed, and nothing else changed.
# Returns one line per problem; prints warnings for changes that aren't
# PingPath's.
function Find-Leftover([string] $InstallDir, $Before) {
    $problems = [Collections.Generic.List[string]]::new()

    # Deleting a service completes once every handle to it is closed.
    $null = Wait-Until $ServiceTimeoutSeconds { $null -eq (Get-PingPathService) }
    foreach ($line in Find-PingPath $InstallDir) {
        $problems.Add("left behind: $line")
    }

    $after = Get-Snapshot
    Save-Snapshot $after 'after'
    foreach ($key in $Before.Keys) {
        $diff = Compare-List $Before[$key] $after[$key]
        if ($key -in 'Services', 'Drivers') {
            # Runners update themselves while jobs run (new versions of
            # updater services, a scanner's driver), and the installer
            # adds the WebView2 runtime with Edge's updater when the runner
            # lacks it. Only PingPath's or Wintun's count here; the rest is
            # shown.
            $ours = @($diff.Added | Where-Object { $_ -match 'pingpath|wintun' })
            foreach ($line in $ours) {
                $problems.Add("${key}: added since before installing: $line")
            }
            foreach ($line in @($diff.Added | Where-Object { $ours -notcontains $_ })) {
                Write-Host "::warning::${key}: added while the check ran, not PingPath's: $line"
            }
            foreach ($line in $diff.Removed) {
                Write-Host "::warning::${key}: gone while the check ran: $line"
            }
            continue
        }
        foreach ($line in $diff.Added) {
            $problems.Add("${key}: added since before installing: $line")
        }
        foreach ($line in $diff.Removed) {
            $problems.Add("${key}: gone since before installing: $line")
        }
    }
    return @($problems)
}

# ---- The check ---------------------------------------------------------------

New-Item -ItemType Directory -Force -Path $Diagnostics | Out-Null
# .NET resolves relative paths against the process's folder, not PowerShell's.
$Diagnostics = (Resolve-Path -LiteralPath $Diagnostics).Path

try {
    Write-Step 'Before installing'
    Assert-Elevated
    $setup = (Resolve-Path -LiteralPath $Installer).Path
    $already = @(Find-PingPath $null)
    if ($already.Count -gt 0) {
        throw "this check needs a machine without PingPath, and this one has: $($already -join '; ')"
    }
    $webView2 = Get-ItemPropertyValue -LiteralPath $WebView2Key -Name pv -ErrorAction SilentlyContinue
    if ($webView2) {
        Write-Host "WebView2 runtime $webView2 is installed"
    } else {
        Write-Host 'the WebView2 runtime is not installed; the installer will install it'
    }
    $before = Get-Snapshot
    Save-Snapshot $before 'before'
    foreach ($key in $before.Keys) {
        Write-Host ('{0}: {1}' -f $key, @($before[$key]).Count)
    }

    Write-Step "Install: $setup"
    Invoke-Installer $setup 'the installer' @('install')
    $first = Assert-Installed 'after installing'
    Show-ServiceLog

    Write-Step 'Leave a relay host route behind, as a crash would'
    Add-LeftoverRoute $LeftoverRoutes[0]

    Write-Step 'Install again, over the running service'
    Invoke-Installer $setup 'the installer, run again,' @('stop', 'install')
    $second = Assert-Installed 'after installing again'
    if ($second.ProcessId -eq $first.ProcessId) {
        throw "after installing again: the service is still process $($first.ProcessId), so the installer didn't stop it to replace its files and start it again"
    }
    Assert-LeftoverRemoved $LeftoverRoutes[0]
    Show-ServiceLog

    Write-Step 'Leave another relay host route behind'
    Add-LeftoverRoute $LeftoverRoutes[1]

    Write-Step 'Uninstall'
    # Uninstalling deletes the logs with the data folder.
    Save-ServiceLog
    Invoke-Uninstaller $second.Dir

    Write-Step 'After uninstalling'
    $problems = @(Find-Leftover $second.Dir $before)
    if ($problems.Count -gt 0) {
        foreach ($problem in $problems) {
            Write-Host "PROBLEM: $problem"
        }
        throw "$($problems.Count) problem(s) after uninstalling; see PROBLEM above"
    }
    Write-Host 'Nothing is left: no service, adapter, network device, PingPath route, data or install folder, shortcut or uninstaller entry; routes, adapters and network devices match the snapshot; no service or driver of PingPath''s or Wintun''s was added.'
} catch {
    Write-Host "::error::installer check: $($_.Exception.Message)"
    Write-Host ($_.ScriptStackTrace)
    Show-Diagnostics
    exit 1
}
exit 0
