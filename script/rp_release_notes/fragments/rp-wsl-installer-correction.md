## RP Windows and WSL correction

- Renames the side-by-side unsigned RP installation to `Zed-RP` while preserving its fork-specific installer and runtime identities.
- Fixes WSL connections to download and verify the exact RP Linux remote server asset that matches the running client commit.
- RP release `20260903.2` cannot provision a WSL remote server because its Windows client incorrectly attempts a development source build; install this corrective release with the RP installer before reconnecting to WSL.
