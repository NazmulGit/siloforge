---
title: Notifications
section: Automation
order: 4
---

# Notifications

SiloForge can tell you when something happens: a job failed, a plan is ready, a backup failed, the PC is running out
of memory. The bell in the top bar shows recent notifications (admins).

## Add a channel

1. Open **Settings → Notifications** and click to add a channel.
2. Pick a **kind** and fill in its fields:

   | Kind | Sends to | You need |
   |---|---|---|
   | `webhook` | Any URL (JSON POST) | The URL; optional extra headers (JSON) |
   | `ntfy` | Phones and desktops through an ntfy topic | Topic URL, for example `https://ntfy.sh/your-topic`; optional access token |
   | `telegram` | A Telegram chat or group, from your bot | Bot token and chat ID; optional topic/thread ID |
   | `hermes` | A Hermes gateway webhook route, which can deliver to Telegram or wake an agent | Route URL; HMAC secret (recommended) |
   | `desktop` | A Windows toast on the PC running SiloForge | Nothing |
   | `log` | The SiloForge log only | Nothing |

3. Tick the **events** you want.
4. Save, then click **Test** to send a test message.

Secrets (bot tokens, webhook secrets) are stored encrypted and never shown again.

## Events

| Event | When |
|---|---|
| `job.failed` | Any job failed |
| `job.done` | Any job finished |
| `plan.ready` | A plan finished, with the number of new suggestions |
| `inject.done` | An inject finished (applied / marked / failed counts) |
| `backup.done` | A backup was written |
| `backup.failed` | A backup failed |
| `system.alert` | CPU, memory or free disk crossed a threshold |

A sensible default is `job.failed`, `backup.failed` and `system.alert`. Each event type is rate limited, so a burst of
failures does not flood you.

## Alert thresholds

System alerts use the thresholds in **Settings → Resources & Performance** (monitor): CPU 95 %, memory 92 %, free disk
below 2,048 MB by default.

> **Tip:** With the `hermes` kind, SiloForge never holds your Telegram bot token: the Hermes gateway delivers the
> message. The gateway route needs the same secret as the channel.
