---
title: Troubleshooting
section: Help
order: 1
---

# Troubleshooting

Start with two checks:

1. Open `http://127.0.0.1:8600/health`. If it answers, SiloForge is running; `jobs.running` shows active jobs.
2. Run `D:\Final_Apps\SiloForge\current\siloforge.exe doctor --home D:\Final_Apps\SiloForge\data`. It checks settings,
   the database, embeddings, ports and disk, and says what failed.

Then look at **Workspace → Logs** (filter status `error`) and **System → errors**.

## Common problems

| Symptom | Likely cause | Fix |
|---|---|---|
| UI does not load | SiloForge is not running | Run the task: `schtasks /run /tn siloforge-keepalive`. If it still does not answer, start `siloforge.exe serve --home ...` in a console to see the error. |
| "Port forbidden" / cannot bind 8600 | Another program or a Windows reserved port range holds the port | Find and stop the other program; check `netsh int ipv4 show excludedportrange tcp`. |
| Login refused, "account locked" | Too many failed sign-ins | Wait (lockout grows from 30 s up to 15 min). Check for a script using an old password. |
| First user cannot register | Registration of the first user only works from the PC itself | Open `http://127.0.0.1:8601`, or create the user with `siloforge user set-password <email> --create`. |
| Crawl gets 401 | Application password missing or wrong, or disabled on plain `http` | Set the credentials again (see [Sites](sites)); use `https`. |
| Crawl blocked for a local/LAN site | Private-host safety guard | An admin allows private hosts for that site. |
| Plan is slow the first time | Every paragraph must be embedded once | Let it finish; later plans take seconds. Check that the GPU is used (Resources & Performance → active device). |
| Embeddings use the CPU, not the GPU | GPU driver problem | Update the graphics driver. SiloForge keeps working on the CPU. |
| `doctor`: embeddings FAIL, runtime or model missing | `runtime\` or `models\` folder empty | `siloforge fetch-runtime` and `siloforge fetch-model`. |
| Many `content_gap` suggestions | No AI chat model configured | Add a chat model chain in [AI models](ai-models), or ignore them. |
| Inject: link stays approved, "anchor not found" | The post was edited in WordPress after the plan | Re-crawl and re-plan, or edit the anchor. |
| Inject: `conflict` | The post changed twice while SiloForge was writing | Run the inject again. Your WordPress edit is never overwritten. |
| Inject: link marked, not applied | No WordPress credentials, or `content_gap` without AI | Add credentials, or use the plugin. |
| Inject: `failed` after verify | The live page did not show the link, or content was lost | The post was rolled back. Check caching plugins and page builders, then retry a single link. |
| Enqueue answers 429 | The site already has 100 queued jobs | Wait, or cancel stale jobs in **All jobs**. |
| "Feature not available on this server" | The server is older than the UI, or a service failed to start | Upgrade; check **System** and the log. |
| Search Console sync: "not connected" | No credentials for the site | Connect Search Console for the site. |
| Agent tool returns 401 | Token revoked or expired | Create a new token and update the agent config. |
| Agent tool missing or "not allowed" | Disabled in the MCP tool manager, or token scope too low | Check **Settings → API & MCP → MCP tools** and the token's scopes. |
| Agent says "confirm required" | Destructive tool called without `confirm: true` | Intended. Review what it will do, then let the agent confirm. |
| Settings saved but nothing changed | The key needs a restart | Click **Restart** when the page says "Restart required". |
| High memory after a plan | Site vectors are cached | Normal, bounded by the vector cache; lower it or set a memory cap. |
| `database is locked` in the log | Another tool writes to `siloforge.db` | Never write to the database with other tools. Reading is fine. |
| Restore refuses | The server is running, or the backup is from a newer version | Stop SiloForge first; install the newer version first. |

## Stopping SiloForge safely

Disable the task first, or it starts SiloForge again within 5 minutes:

```bat
schtasks /change /tn siloforge-keepalive /disable
taskkill /F /PID <number from D:\Final_Apps\SiloForge\data\siloforge.pid>
```

Start again with `schtasks /change /tn siloforge-keepalive /enable` and `schtasks /run /tn siloforge-keepalive`.
Stopping is safe for the data, except during an inject.
