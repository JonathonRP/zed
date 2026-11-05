$ErrorActionPreference = "Stop"
. "$PSScriptRoot\lib\rp-release-metadata.ps1"

function Assert-Fails {
    param([scriptblock]$Command, [string]$Message)

    $failed = $false
    try {
        & $Command
    } catch {
        $failed = $true
    }
    if (-not $failed) {
        throw $Message
    }
}

Invoke-WithRpReleaseMetadata {
    Get-ChildItem Env: | Where-Object Name -Like "ZED_RP_*" | ForEach-Object {
        Remove-Item -LiteralPath "Env:$($_.Name)"
    }
    Assert-RpReleaseMetadata
    Invoke-WithRpReleaseMetadata {
        $env:ZED_RP_RELEASE_VERSION = "unexpected"
    }
    if (Get-ChildItem Env: | Where-Object Name -Like "ZED_RP_*") {
        throw "Developer shell must not introduce RP metadata into an official build"
    }

    $notes = "# RP release notes`n`nPreserve equals=signs, quotes `"and`" all lines $([char]0x2192).`n"
    $notesHash = [Convert]::ToHexString(
        [System.Security.Cryptography.SHA256]::HashData([System.Text.Encoding]::UTF8.GetBytes($notes))
    ).ToLowerInvariant()
    $manifest = [ordered]@{
        schema_version = 2
        channel = "rp-stable"
        calendar_version = "20261028.2"
        tag = "rp-stable-20261028.2"
        upstream_version = "1.22.0"
        upstream_tag = "v1.22.0"
        upstream_tag_commit = "a" * 40
        commit = "b" * 40
        trust = @{ signed = $false; label = "unsigned" }
        notes_identity = "sha256:$notesHash"
    } | ConvertTo-Json -Depth 10
    $expected = @{
        ZED_RP_RELEASE_VERSION = "20261028.2"
        ZED_RP_RELEASE_TAG = "rp-stable-20261028.2"
        ZED_RP_UPSTREAM_TAG = "v1.22.0"
        ZED_RP_UPSTREAM_TAG_COMMIT = "a" * 40
        ZED_RP_RELEASE_NOTES = $notes
        ZED_RP_RELEASE_NOTES_IDENTITY = "sha256:$notesHash"
        ZED_RP_RELEASE_MANIFEST = $manifest
    }
    foreach ($name in $expected.Keys) {
        [Environment]::SetEnvironmentVariable($name, $expected[$name], "Process")
    }
    Assert-RpReleaseMetadata

    Invoke-WithRpReleaseMetadata {
        # Match the lossy line-oriented environment import in Launch-VsDevShell.
        Get-ChildItem Env: | Where-Object Name -Like "ZED_RP_*" | ForEach-Object {
            [Environment]::SetEnvironmentVariable($_.Name, ($_.Value -split "\r?\n")[0], "Process")
        }
        Assert-Fails { Assert-RpReleaseMetadata } "A truncated manifest must fail before Cargo"
        $env:ZED_RP_INJECTED = "unexpected"
    }
    Assert-RpReleaseMetadata
    foreach ($name in $expected.Keys) {
        if ([Environment]::GetEnvironmentVariable($name) -cne $expected[$name]) {
            throw "$name was not preserved byte-for-byte"
        }
    }
    if (Test-Path Env:ZED_RP_INJECTED) {
        throw "Developer shell introduced unexpected RP metadata"
    }

    Assert-Fails {
        Invoke-WithRpReleaseMetadata {
            $env:ZED_RP_RELEASE_NOTES = ($notes -split "`n")[0]
            Assert-RpReleaseMetadata
        }
    } "Truncated release notes must fail their SHA-256 identity"
    Assert-Fails {
        Invoke-WithRpReleaseMetadata {
            $env:ZED_RP_UPSTREAM_TAG = "v1.25.3"
            Assert-RpReleaseMetadata
        }
    } "Mismatched upstream identity must fail before Cargo"
    Assert-Fails {
        Invoke-WithRpReleaseMetadata {
            Remove-Item Env:ZED_RP_RELEASE_MANIFEST
            Assert-RpReleaseMetadata
        }
    } "Incomplete metadata must not become an official build"
    Assert-Fails {
        Invoke-WithRpReleaseMetadata {
            $env:ZED_RP_RELEASE_NOTES = "changed"
            throw "Simulated developer-shell failure"
        }
    } "Developer-shell failures must propagate"
    Assert-RpReleaseMetadata

    $temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) "zed-rp-metadata-$([guid]::NewGuid())"
    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
    try {
        $source = @'
const fn equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() { return false; }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] { return false; }
        index += 1;
    }
    true
}
'@
        foreach ($name in $expected.Keys) {
            $expectedPath = Join-Path $temporaryDirectory "$name.txt"
            [System.IO.File]::WriteAllText($expectedPath, $expected[$name], [System.Text.UTF8Encoding]::new($false))
            $source += "`nconst _: () = assert!(equal(option_env!(`"$name`").unwrap().as_bytes(), include_bytes!(r#`"$expectedPath`"#)));"
        }
        $sourcePath = Join-Path $temporaryDirectory "embedded_metadata.rs"
        [System.IO.File]::WriteAllText($sourcePath, $source)
        $outputPath = Join-Path $temporaryDirectory "embedded_metadata.rmeta"
        & rustc --edition 2024 --crate-type lib --emit metadata -o $outputPath $sourcePath
        if ($LASTEXITCODE -ne 0) {
            throw "Rust option_env metadata probe failed"
        }
        if (-not (Test-Path -LiteralPath $outputPath -PathType Leaf)) {
            throw "Rust option_env metadata probe did not produce an artifact"
        }
    } finally {
        Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force
    }
}

Write-Output "RP multiline environment and Rust compile-time metadata assertions passed"
