---
title: Backups and migration
section: Administration
order: 3
---

# Backups and migration

All of SiloForge lives in one data folder: `D:\Final_Apps\SiloForge\data` (database, settings, secrets, logs, model and
GPU runtime). Nothing is stored in the registry or your user profile.

## Automatic backups

- Every day at **03:17** (local time) SiloForge writes a backup and keeps the last **10**.
- Change the time, the number kept and the folder in **Settings → Backups**.
- Backups are **encrypted** (AES-256) with the backup passphrase created at install time.

> **Warning:** Keep a copy of the backup passphrase **outside this PC**, for example in your password manager. Without
> it a backup cannot be restored on a new PC. It is in `data\secrets.env` as `SILOFORGE_BACKUP_PASSPHRASE`.

## Make a backup now

1. Open **Settings → Backups**.
2. Pick a kind:

   | Kind | Contains | Use it for |
   |---|---|---|
   | **Full** | Database snapshot + settings + secrets (secrets only when encrypted) | Moving or restoring a whole install |
   | **DB** | Database only | Before an upgrade or a risky change |
   | **Config** | Settings + secrets | Keeping your configuration |

3. Add a note if you like and start it. It is safe while SiloForge is running.
4. **Verify** re-reads an archive and checks every file's checksum. **Download** saves a copy.

The model and GPU runtime folders (`models\`, `runtime\`) are not in backups; they can be downloaded again.

## Restore

Restore is done from the command line, with SiloForge stopped:

1. Stop SiloForge: disable the `siloforge-keepalive` task, then end the `siloforge.exe` process.
2. Run:
   ```powershell
   D:\Final_Apps\SiloForge\current\siloforge.exe restore <backup file> --passphrase <passphrase> --home D:\Final_Apps\SiloForge\data
   ```
3. Enable and start the task again.

Restore is all-or-nothing and keeps the replaced files as `*.pre-restore-<time>`. It refuses a backup from a newer
version of SiloForge and refuses while the server is running.

## Move to another PC

**Option A: copy the folder** (fastest)
1. On the old PC, stop SiloForge (as above).
2. Copy `D:\Final_Apps\SiloForge\data` to the same place on the new PC.
3. On the new PC, unzip the release from [Downloads](downloads) and run `scripts\install.ps1`. It finds the existing
   data and starts SiloForge.

**Option B: backup and restore**
1. On the old PC, make a **Full** backup.
2. On the new PC, install SiloForge, stop it, restore the backup with the **old** passphrase, then start it.
   The installer can fetch the model and runtime, or copy `models\` and `runtime\` over.

After either option: run `siloforge doctor`, check that your WordPress sites are reachable, and update anything that
pointed at the old PC (Hermes MCP config, monitoring). The GPU is detected again automatically.

> **Warning:** Never run the old and the new install at the same time against the same WordPress sites. Both would plan
> and inject on their own.

## Upgrade

Download the new app zip, unzip it and run `scripts\install.ps1`. The installer refuses while jobs run, makes a DB
backup first, installs the new version next to the old one, switches over and checks health. The old version stays in
`D:\Final_Apps\SiloForge\app\<old version>` for a rollback.

## The v1 archive

SiloForge 1.x (the Docker version) was retired. Its final database dump, images and a restore guide are kept in
`D:\Backups\SiloForge\v1-archive`. All v1 data (sites, posts, suggestions, history, vectors, users) was imported into
v2, so you do not need it for daily work.
