# Installs PingPath with its Windows installer, installs it again over the
# running service, uninstalls it, and checks that nothing is left behind
# (PLAN.md Phase 3: "Uninstall leaves no routes, adapters or services
# behind"). CI runs it on windows-latest, whose jobs run as an
# administrator, right after building the installer:
#
#   pwsh -File tools/check-installer.ps1 -Installer <PingPath_..._x64-setup.exe> -WintunSha256 <hex> [-Diagnostics <folder>]
#
# It changes the machine it runs on, so run it only on a throwaway one.
#
# 1. Snapshot: every route (IPv4 and IPv6, active and persistent stores),
#    network adapter (hidden ones too), network device (non-present ones
#    too), service and driver.
# 2. Install silently and check: exit code 0; the files are in place,
#    wintun.dll among them with its pinned SHA-256; the PingPath service
#    is registered to start automatically, from the install folder, and
#    runs; its named pipe is up; %ProgramData%\PingPath exists.
# 3. Install again: the installer's hooks must stop the running service
#    before replacing its files, as an upgrade does. Same checks.
# 4. Leave what a crashed service leaves: a relay host route (metric 293,
#    through the runner's gateway) recorded in the service's route ledger.
#    Nothing in this check creates the adapter, and a clean install has
#    no routes, so without this the uninstall would have nothing to
#    remove. The route goes to 192.0.2.1 (TEST-NET-1), which carries no
#    real traffic.
# 5. Uninstall silently and check: exit code 0; the service, the adapter,
#    every route on a PingPath interface or with metric 293,
#    %ProgramData%\PingPath, the install folder and the uninstaller's
#    registry key are gone; and routes, adapters, network devices,
#    services and drivers match the snapshot.
#
# Every failure says what was expected and what was found, and prints the
# service's own log (PLAN.md section 9). -Diagnostics gets the snapshots
# and a copy of the service's logs, which uninstalling deletes; CI keeps
# that folder when the check fails.

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
# ADAPTER_GUID in client/core/src/net/windows/adapter.rs and
# HOST_ROUTE_METRIC in client/core/src/net/route_table.rs.
$ServiceName = 'PingPath'
$Pipe = '\\.\pipe\pingpath'
$AdapterGuid = '066C8B67-BABF-4B6D-8AC5-666510895AAB'
$HostRouteMetric = 293
$UninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PingPath'
$DataDir = Join-Path $env:ProgramData 'PingPath'
$LeftoverRoute = '192.0.2.1/32'
# The WebView2 runtime's registration, which the installer checks to
# decide whether to install the runtime.
$WebView2Key = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
# Long enough for the installer to download and install WebView2 when the
# runner lacks it.
$SetupTimeoutSeconds = 600

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
# services. Get-Service would also keep a handle to each service it
# returns, and a service deleted while a handle is open stays "marked for
# deletion" until the handle closes.
function Get-ServiceList {
    return @(Get-CimInstance -ClassName Win32_Service | ForEach-Object Name | Sort-Object -Unique)
}

function Get-DriverList {
    return @(Get-CimInstance -ClassName Win32_SystemDriver | ForEach-Object Name | Sort-Object -Unique)
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
                $_.DestinationPrefix -eq $LeftoverRoute
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

function Get-InstallDir {
    if (-not (Test-Path -LiteralPath $UninstallKey)) {
        throw "the installer registered no uninstaller: $UninstallKey is missing"
    }
    # The installer writes the folder in quotes.
    return (Get-ItemPropertyValue -LiteralPath $UninstallKey -Name InstallLocation).Trim('"')
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

# Everything that could explain a failure. Never throws: it runs when
# something already has.
function Show-Diagnostics {
    $ErrorActionPreference = 'Continue'
    Write-Step 'Diagnostics'
    try {
        Write-Host "-- sc.exe query $ServiceName / qc / qfailure"
        sc.exe query $ServiceName | Write-Host
        sc.exe qc $ServiceName | Write-Host
        sc.exe qfailure $ServiceName | Write-Host
        Write-Host '-- PingPath adapters, network devices and routes'
        Get-PingPathAdapter | Format-List Name, InterfaceDescription, InterfaceGuid, Status | Out-String -Width 300 | Write-Host
        Get-PingPathDevice | Format-List DeviceID, Name, Status | Out-String -Width 300 | Write-Host
        Get-PingPathRoute | Format-Table -AutoSize DestinationPrefix, NextHop, InterfaceAlias, RouteMetric | Out-String -Width 300 | Write-Host
        Write-Host "-- $UninstallKey"
        Get-ItemProperty -LiteralPath $UninstallKey -ErrorAction SilentlyContinue | Format-List | Out-String -Width 300 | Write-Host
        Write-Host '-- PingPath folders'
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
        Write-Host "Snapshots and saved logs: $Diagnostics"
    } catch {
        Write-Host "(diagnostics stopped: $($_.Exception.Message))"
    }
}

# ---- Installing and uninstalling --------------------------------------------

# Runs an NSIS installer or uninstaller and fails on any exit code but 0.
# It waits for that process only, with a time limit: Start-Process -Wait
# would also wait for every process it starts (the WebView2 runtime's
# updater, for one) and could hang the job.
function Invoke-Setup([string] $FilePath, [string] $Arguments, [string] $What) {
    Write-Host "> `"$FilePath`" $Arguments"
    $process = Start-Process -FilePath $FilePath -ArgumentList $Arguments -PassThru
    # Holding the handle keeps the exit code readable after the process ends.
    $null = $process.Handle
    $clock = [Diagnostics.Stopwatch]::StartNew()
    if (-not $process.WaitForExit($SetupTimeoutSeconds * 1000)) {
        $process.Kill()
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

# Checks what installing must leave, and returns the install folder.
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

    $running = Wait-Until 30 {
        $service = Get-PingPathService
        $null -ne $service -and $service.State -eq 'Running'
    }
    $service = Get-PingPathService
    if ($null -eq $service) {
        throw "${When}: no service named $ServiceName is registered"
    }
    Write-Host ('service {0}: {1}, start mode {2}, command line {3}' -f $service.Name, $service.State,
        $service.StartMode, $service.PathName)
    if (-not $running) {
        throw "${When}: the $ServiceName service is $($service.State) 30 s after installing, not Running"
    }
    if ($service.StartMode -ne 'Auto') {
        throw "${When}: the $ServiceName service starts $($service.StartMode), not Auto"
    }
    $exe = Join-Path $dir 'pingpath-service.exe'
    if ($service.PathName.IndexOf($exe, [StringComparison]::OrdinalIgnoreCase) -lt 0) {
        throw "${When}: the $ServiceName service runs $($service.PathName), not $exe"
    }
    # Listing the pipes doesn't connect to any: the service sees nothing.
    $serving = Wait-Until 30 { [IO.Directory]::GetFiles('\\.\pipe\') -contains $Pipe }
    if (-not $serving) {
        throw "${When}: the service runs but $Pipe doesn't exist 30 s after installing"
    }
    Write-Host "$Pipe is up"
    if (-not (Test-Path -LiteralPath $DataDir -PathType Container)) {
        throw "${When}: the service runs but $DataDir doesn't exist"
    }
    return $dir
}

# A relay host route and its ledger record, as a crashed service leaves
# them (client/core/src/net/route_table.rs: RouteRow, Ledger). Uninstalling
# must remove the route because the ledger names it exactly.
function Add-LeftoverRoute {
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
    New-NetRoute -DestinationPrefix $LeftoverRoute -InterfaceIndex $via.InterfaceIndex -NextHop $via.NextHop `
        -RouteMetric $HostRouteMetric -PolicyStore ActiveStore | Out-Null
    $ledger = [ordered]@{
        routes = @(
            [ordered]@{
                interface = [uint64] $adapters[0].NetLuid
                destination = $LeftoverRoute
                next_hop = $via.NextHop
                metric = $HostRouteMetric
            }
        )
    } | ConvertTo-Json -Depth 4
    $path = Join-Path $DataDir 'state\routes.json'
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null
    # UTF-8 without a byte order mark, as the service writes it.
    [IO.File]::WriteAllText($path, $ledger)
    Write-Host "route to $LeftoverRoute via $($via.NextHop) on $($via.InterfaceAlias), metric $HostRouteMetric; ${path}:"
    Write-Host $ledger
    $added = @(Get-NetRoute -PolicyStore ActiveStore | Where-Object {
            $_.DestinationPrefix -eq $LeftoverRoute -and $_.RouteMetric -eq $HostRouteMetric
        })
    if ($added.Count -ne 1) {
        throw "the leftover route to $LeftoverRoute isn't in the route table after adding it"
    }
}

# An NSIS uninstaller copies itself to %TEMP%, starts the copy and exits at
# once, so its exit code says nothing. Run as `uninstall.exe /S _?=<folder>`
# it uninstalls in place and returns its own exit code, but it can't delete
# itself, so the folder would stay. A copy run from elsewhere with _?= does
# both, as NSIS's own hand-off does. _?= must come last, unquoted.
function Invoke-Uninstaller([string] $InstallDir) {
    $copyDir = Join-Path ([IO.Path]::GetTempPath()) "pingpath-uninstall-$([guid]::NewGuid())"
    New-Item -ItemType Directory -Path $copyDir | Out-Null
    $copy = Join-Path $copyDir 'uninstall.exe'
    Copy-Item -LiteralPath (Join-Path $InstallDir 'uninstall.exe') -Destination $copy
    try {
        Invoke-Setup -FilePath $copy -Arguments "/S _?=$InstallDir" -What 'the uninstaller'
    } finally {
        Remove-Item -LiteralPath $copyDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Everything uninstalling must have removed, and nothing else changed.
# Returns one line per problem.
function Find-Leftover([string] $InstallDir, $Before) {
    $problems = [Collections.Generic.List[string]]::new()

    # Deleting a service completes once every handle to it is closed.
    if (-not (Wait-Until 30 { $null -eq (Get-PingPathService) })) {
        $service = Get-PingPathService
        $state = if ($null -ne $service) { $service.State } else { 'gone just now' }
        $problems.Add("the $ServiceName service is still registered ($state) 30 s after uninstalling")
    }
    foreach ($adapter in Get-PingPathAdapter) {
        $problems.Add("an adapter is left: $($adapter.Name) ($($adapter.InterfaceDescription), $($adapter.InterfaceGuid))")
    }
    foreach ($device in Get-PingPathDevice) {
        $problems.Add("a network device is left: $($device.DeviceID) ($($device.Name), $($device.Status))")
    }
    foreach ($route in Get-PingPathRoute) {
        $what = if ($route.DestinationPrefix -eq $LeftoverRoute) {
            "the relay host route left as a crash would, which the service's ledger names"
        } else {
            'a route on a PingPath interface or with metric 293'
        }
        $problems.Add(('{0} is left: {1} via {2} on {3}, metric {4}' -f $what, $route.DestinationPrefix,
                $route.NextHop, $route.InterfaceAlias, $route.RouteMetric))
    }
    foreach ($folder in @(Get-ChildItem -LiteralPath $env:ProgramData -Filter 'PingPath*' -Force)) {
        $problems.Add("a data folder is left: $($folder.FullName)")
    }
    if (Test-Path -LiteralPath $InstallDir) {
        $files = @(Get-ChildItem -LiteralPath $InstallDir -Recurse -Force | ForEach-Object Name) -join ', '
        $problems.Add("the install folder is left: $InstallDir (holding: $files)")
    }
    if (Test-Path -LiteralPath $UninstallKey) {
        $problems.Add("the uninstaller's registry key is left: $UninstallKey")
    }

    $after = Get-Snapshot
    Save-Snapshot $after 'after'
    foreach ($key in $Before.Keys) {
        $diff = Compare-List $Before[$key] $after[$key]
        $added = $diff.Added
        if ($key -eq 'Services') {
            # The installer installs the WebView2 runtime when it is
            # missing, with Edge's updater; other apps share them, so they
            # rightly stay.
            $edge = @($added | Where-Object { $_ -like 'edgeupdate*' -or $_ -like 'MicrosoftEdge*' })
            if ($edge.Count -gt 0) {
                Write-Host "new services from the WebView2 runtime, allowed: $($edge -join ', ')"
            }
            $added = @($added | Where-Object { $edge -notcontains $_ })
        }
        foreach ($line in $added) {
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
$setup = (Resolve-Path -LiteralPath $Installer).Path

try {
    Write-Step 'Before installing'
    if ($null -ne (Get-PingPathService)) {
        throw "a $ServiceName service is already registered; this check needs a machine without PingPath"
    }
    if (Test-Path -LiteralPath $DataDir) {
        throw "$DataDir already exists; this check needs a machine without PingPath"
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
    Invoke-Setup -FilePath $setup -Arguments '/S' -What 'the installer'
    $installDir = Assert-Installed 'after installing'
    Show-ServiceLog

    Write-Step 'Install again, over the running service'
    Invoke-Setup -FilePath $setup -Arguments '/S' -What 'the installer, run again'
    $installDir = Assert-Installed 'after installing again'

    Write-Step 'Leave a relay host route behind, as a crash would'
    Add-LeftoverRoute

    Write-Step 'Uninstall'
    # Uninstalling deletes the logs with the data folder.
    Save-ServiceLog
    Invoke-Uninstaller $installDir

    Write-Step 'After uninstalling'
    $problems = @(Find-Leftover $installDir $before)
    if ($problems.Count -gt 0) {
        foreach ($problem in $problems) {
            Write-Host "LEFT BEHIND: $problem"
        }
        throw "$($problems.Count) problem(s) after uninstalling; see LEFT BEHIND above"
    }
    Write-Host 'Nothing is left: no service, adapter, network device, PingPath route, data or install folder, and routes, adapters, devices, services and drivers match the snapshot.'
} catch {
    Write-Host "::error::installer check: $($_.Exception.Message)"
    Write-Host ($_.ScriptStackTrace)
    Show-Diagnostics
    exit 1
}
exit 0
