$ErrorActionPreference = "Stop"

function Assert-Matches {
    param(
        [string]$Text,
        [string]$Pattern,
        [string]$Message
    )

    if ($Text -notmatch $Pattern) {
        throw $Message
    }
}

function Get-Assignment {
    param(
        [string]$Text,
        [string]$Name
    )

    return [regex]::Match(
        $Text,
        "\`$$Name\s*=\s*`"(?<value>[^`"]*)`""
    ).Groups["value"].Value
}

function Resolve-RpGuards {
    param(
        [string]$Text,
        [bool]$RpPackage
    )

    $result = [System.Collections.Generic.List[string]]::new()
    $states = [System.Collections.Generic.Stack[object]]::new()
    $active = $true
    foreach ($line in $Text -split "\r?\n") {
        if ($line -match '^#ifdef RpPackage$') {
            $states.Push([pscustomobject]@{ ParentActive = $active; Condition = $RpPackage })
            $active = $active -and $RpPackage
            continue
        }
        if ($line -match '^#ifndef RpPackage$') {
            $states.Push([pscustomobject]@{ ParentActive = $active; Condition = -not $RpPackage })
            $active = $active -and (-not $RpPackage)
            continue
        }
        if ($line -eq "#else" -and $states.Count -gt 0) {
            $state = $states.Peek()
            $active = $state.ParentActive -and (-not $state.Condition)
            continue
        }
        if ($line -eq "#endif" -and $states.Count -gt 0) {
            $active = $states.Pop().ParentActive
            continue
        }
        if ($active) {
            $result.Add($line)
        }
    }
    if ($states.Count -ne 0) {
        throw "Unbalanced RpPackage preprocessor guards"
    }
    return $result -join "`n"
}

$root = Split-Path -Parent $PSScriptRoot
$bundle = Get-Content -Raw "$root\script\bundle-windows.ps1"
$installer = Get-Content -Raw "$root\crates\zed\resources\windows\zed.iss"
$releaseChannel = Get-Content -Raw "$root\crates\release_channel\src\lib.rs"
$releaseWorkflow = Get-Content -Raw "$root\.github\workflows\fork_stable_release.yml"

$officialBlock = [regex]::Match(
    $bundle,
    '(?s)function BuildInstaller.*?switch \(\$channel\)\s*\{\s*"stable"\s*\{(?<body>.*?)\}\s*"preview"'
).Groups["body"].Value
$rpBlock = [regex]::Match(
    $bundle,
    '(?s)if \(\$isRpPackage\) \{\s+if \(\$channel -ne "stable"\).*?(?<body>\$appId = .*?)\s+\}'
).Groups["body"].Value

foreach ($name in @("appId", "appName", "appDisplayName", "appMutex", "regValueName", "appUserId")) {
    $officialValue = Get-Assignment $officialBlock $name
    $rpValue = Get-Assignment $rpBlock $name
    if (-not $officialValue -or -not $rpValue -or $officialValue -eq $rpValue) {
        throw "RP and official stable $name values must both exist and be distinct"
    }
}

$rpIdentifier = [regex]::Match(
    $releaseChannel,
    'const RP_APP_IDENTIFIER: &str = "(?<id>[^"]+)"'
).Groups["id"].Value
if (-not $rpIdentifier) {
    throw "RP runtime application identifier is missing"
}

Assert-Matches $bundle ([regex]::Escape("`$appMutex = `"$rpIdentifier-Instance-Mutex`"")) `
    "Installer mutex must match the runtime RP identifier"
Assert-Matches $bundle '(?s)\$appName = "Zed-RP".*?\$appDisplayName = "Zed-RP \(Unsigned RP Stable\)".*?\$regValueName = "ZedRPStable".*?\$appUserId = "Zed-ACP-Patched-RP-Stable"' `
    "RP package identities are incomplete"
Assert-Matches $bundle '(?s)if \(\$isRpPackage\).*?\$definitions\["RpPackage"\] = "1"' `
    "RP builds must define the Inno preprocessor guard"
Assert-Matches $bundle '(?s)identity=Zed-ACP-Patched-RP-Stable.*?zed-rp-installer\.marker' `
    "RP marker source must be generated"
Assert-Matches $bundle '(?s)\$cargoArguments = @\(\$Arguments\).*?\$cargoArguments \+= @\("--config", \$script:cargoRustcWrapperConfig\).*?cargo @cargoArguments' `
    "The required sccache wrapper configuration must have final Cargo precedence"
Assert-Matches $bundle '\$env:SCCACHE_IDLE_TIMEOUT = "0"' `
    "RP Windows packaging must retain sccache statistics across long linker phases"
Assert-Matches $bundle '(?s)--show-stats --stats-format json \| Out-Null.*?--stop-server \| Out-Null.*?--start-server.*?--zero-stats' `
    "RP Windows packaging must restart sccache with the disabled idle timeout"
Assert-Matches $releaseWorkflow '(?s)package_windows:.*?Set up sccache.*?disable_annotations: true.*?Pin sccache wrapper path' `
    "RP Windows packaging must disable the action post-run that would restart sccache after cleanup"
Assert-Matches $bundle '(?s)function Test-RpSccacheProductionPath.*?"--config", "\.cargo/bundle-config\.toml".*?"--package", "refineable".*?production Cargo path bypassed sccache' `
    "RP Windows packaging must fail fast when the production Cargo path bypasses sccache"
Assert-Matches $bundle '(?s)GenerateLicenses.*?statistics reset after license generation.*?Test-RpSccacheProductionPath.*?BuildZedAndItsFriends' `
    "The production-path sccache preflight must run before the expensive Windows build"
if ($releaseWorkflow.Contains("remote/build-remote-server-binary")) {
    throw "RP release clients must download the matching published remote server, not build one from source"
}

Assert-Matches $installer '(?s)#ifdef RpPackage\s+AppPublisher=JonathonRP.*?AppPublisherURL=https://github\.com/JonathonRP/zed.*?AppSupportURL=https://github\.com/JonathonRP/zed/issues.*?AppUpdatesURL=https://github\.com/JonathonRP/zed/releases\s+#else\s+AppPublisher=Zed Industries' `
    "RP publisher URLs must be fork-owned without changing official values"
Assert-Matches $installer '(?s)#ifdef RpPackage\s+ChangesEnvironment=false\s+ChangesAssociations=false\s+#else\s+ChangesEnvironment=true\s+ChangesAssociations=true\s+#endif' `
    "RP integration change flags must be disabled"
Assert-Matches $installer '(?s)PrivilegesRequired=lowest\s+#ifdef RpPackage\s+UsePreviousAppDir=yes\s+#endif' `
    "RP privilege and previous directory policy is missing"
Assert-Matches $installer '(?s)\[Tasks\]\s+Name: "desktopicon".*?#ifndef RpPackage.*?Name: "addcontextmenufiles".*?Name: "associatewithfiles".*?Name: "addtopath".*?#endif' `
    "RP integration tasks must be guarded"
Assert-Matches $installer '(?s)#ifdef RpPackage\s+Source:.*?zed-rp-installer\.marker.*?DestName: "\.zed-rp-installer".*?#else\s+Source:.*?\\appx\\\*' `
    "RP marker and Appx exclusion must share an exclusive guard"
Assert-Matches $installer '(?s)\[UninstallRun\]\s+#ifndef RpPackage.*?Remove-AppxPackage.*?#endif\s+\[Registry\]\s+#ifndef RpPackage.*?; URI Scheme.*?Software\\Classes\\zed.*?#endif\s+\[Code\]' `
    "Appx removal, associations, context menus, PATH, and zed:// registration must be excluded"
Assert-Matches $installer '(?s)AppId=\{#AppId\}.*?AppVerName=\{#AppDisplayName\}.*?DefaultGroupName=\{#AppName\}.*?\[Icons\]\s+Name: "\{group\}\\\{#AppName\}".*?AppUserModelID: "\{#AppUserId\}"' `
    "Uninstall, program group, shortcut, and AUMID identities must use isolated definitions"

$rpInstaller = Resolve-RpGuards $installer $true
$officialInstaller = Resolve-RpGuards $installer $false
if ($rpInstaller -match 'PrivilegesRequiredOverridesAllowed\s*=\s*(commandline|dialog)') {
    throw "RP installer must not allow privilege overrides"
}
foreach ($integration in @(
    'Name: "associatewithfiles"',
    'Name: "addcontextmenufiles"',
    'Name: "addtopath"',
    'Source: "{#ResourcesDir}\appx\*"',
    'Subkey: "Environment"',
    'Software\Classes\zed',
    "Add-AppxPackage",
    "Remove-AppxPackage"
)) {
    if ($rpInstaller.Contains($integration)) {
        throw "RP installer unexpectedly contains integration: $integration"
    }
    if (-not $officialInstaller.Contains($integration)) {
        throw "Official installer unexpectedly lost integration: $integration"
    }
}
if (-not $rpInstaller.Contains('DestName: ".zed-rp-installer"') -or
    $officialInstaller.Contains('DestName: ".zed-rp-installer"')) {
    throw "RP marker must only be installed by RP packages"
}

Write-Output "Windows RP packaging identity assertions passed"
