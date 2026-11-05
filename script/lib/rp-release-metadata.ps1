function Invoke-WithRpReleaseMetadata {
    param([Parameter(Mandatory)][scriptblock]$Command)

    $metadata = @{}
    Get-ChildItem Env: | Where-Object Name -Like "ZED_RP_*" | ForEach-Object {
        $metadata[$_.Name] = $_.Value
    }

    try {
        & $Command
    } finally {
        # VS imports line-oriented `set` output, truncating multiline values.
        Get-ChildItem Env: | Where-Object Name -Like "ZED_RP_*" | ForEach-Object {
            Remove-Item -LiteralPath "Env:$($_.Name)"
        }
        foreach ($name in $metadata.Keys) {
            [Environment]::SetEnvironmentVariable($name, $metadata[$name], "Process")
        }
    }
}

function Assert-RpReleaseMetadata {
    if (-not (Get-ChildItem Env: | Where-Object Name -Like "ZED_RP_*")) {
        return
    }

    foreach ($name in @(
        "ZED_RP_RELEASE_VERSION",
        "ZED_RP_RELEASE_TAG",
        "ZED_RP_UPSTREAM_TAG",
        "ZED_RP_UPSTREAM_TAG_COMMIT",
        "ZED_RP_RELEASE_NOTES",
        "ZED_RP_RELEASE_NOTES_IDENTITY",
        "ZED_RP_RELEASE_MANIFEST"
    )) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
            throw "RP compile-time metadata is missing $name"
        }
    }

    $manifest = $env:ZED_RP_RELEASE_MANIFEST | ConvertFrom-Json -AsHashtable -ErrorAction Stop
    if ($manifest.schema_version -ne 2 -or
        $manifest.channel -cne "rp-stable" -or
        $manifest.calendar_version -cne $env:ZED_RP_RELEASE_VERSION -or
        $manifest.tag -cne $env:ZED_RP_RELEASE_TAG -or
        $manifest.tag -cne "rp-stable-$env:ZED_RP_RELEASE_VERSION" -or
        $manifest.upstream_tag -cne $env:ZED_RP_UPSTREAM_TAG -or
        $manifest.upstream_tag -cne "v$($manifest.upstream_version)" -or
        $manifest.upstream_tag_commit -cne $env:ZED_RP_UPSTREAM_TAG_COMMIT -or
        $manifest.upstream_tag_commit -cnotmatch '^[0-9a-f]{40}$' -or
        $manifest.commit -cnotmatch '^[0-9a-f]{40}$' -or
        $manifest.notes_identity -cne $env:ZED_RP_RELEASE_NOTES_IDENTITY) {
        throw "RP compile-time metadata does not match the release manifest"
    }

    $notesBytes = [System.Text.Encoding]::UTF8.GetBytes($env:ZED_RP_RELEASE_NOTES)
    $notesHash = [Convert]::ToHexString(
        [System.Security.Cryptography.SHA256]::HashData($notesBytes)
    ).ToLowerInvariant()
    if ($env:ZED_RP_RELEASE_NOTES_IDENTITY -cne "sha256:$notesHash") {
        throw "RP compile-time release notes do not match their SHA-256 identity"
    }
}
