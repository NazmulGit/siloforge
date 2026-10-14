---
title: Downloads
section: Administration
order: 5
---

# Downloads

**Workspace → Downloads** offers the installers for SiloForge and the WordPress plugin, straight from your own server.

## What you see

- **Latest app**: `siloforge-<version>-windows-x64.zip`, with version, size and SHA-256 checksum.
- **Latest plugin**: `siloforge-connect-<version>.zip` (see [WordPress plugin](wordpress-plugin)).
- **Older versions**, in case you need to roll back.
- The version you are running now.

Files come from the releases folder, `D:\Final_Apps\SiloForge\releases` by default (`[downloads] dir` in the settings
file).

## Check a download

The SHA-256 on the page must match the file. In PowerShell:

```powershell
Get-FileHash .\siloforge-2.1.0-windows-x64.zip -Algorithm SHA256
```

## Install or upgrade the app

1. Download the app zip and unzip it.
2. Run `powershell -ExecutionPolicy Bypass -File .\siloforge-<version>\scripts\install.ps1`.
3. The installer backs up the database, installs to `D:\Final_Apps\SiloForge\app\<version>`, switches the `current`
   link to it, registers the `siloforge-keepalive` task and checks that the server answers.

On a brand-new PC, create the first admin afterwards:

```powershell
$env:SILOFORGE_NEW_PASSWORD = '<a strong password>'
D:\Final_Apps\SiloForge\current\siloforge.exe user set-password admin@siloforge.local --create --home D:\Final_Apps\SiloForge\data
```

> **Tip:** Upgrades keep all your data. The previous version stays installed so you can switch back.
