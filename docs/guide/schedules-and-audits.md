---
title: Schedules and audits
section: Automation
order: 1
---

# Schedules and audits

Schedules run jobs for a site automatically. They never inject: scheduled work only reads your site and creates
suggestions.

## Job kinds you can schedule

| Kind | What it does | Typical schedule |
|---|---|---|
| `audit` | The full loop: crawl → link health → plan → metrics snapshot (and Search Console, if connected). | Weekly. If you schedule only one thing, schedule this. |
| `crawl` | Re-reads the content only. Cheap. | Daily on busy sites |
| `plan` | Re-scores candidates over the content already indexed. | After crawls |
| `link_health` | Re-checks every link's status, redirects and `rel`. Payload `{"internal_only": true}` checks internal links only. | Weekly |

## Add a schedule

1. Open the site's **Jobs** page and find **Add / update a schedule**.
2. Pick the **kind**.
3. Enter a **cron** expression, or click a preset: **Daily 3am** (`0 3 * * *`), **Weekly Mon 3am** (`0 3 * * 1`),
   **Monthly 1st 3am** (`0 3 1 * *`).
4. Pick the **timezone** (for example `UTC`, `Europe/London`, `Asia/Dhaka`). Your browser's zone is pre-filled.
5. For `audit` and `plan`, pick a topology or leave **site default**.
6. Save. There is one schedule per site and kind; saving the same kind again replaces it.

Cron has five fields: minute, hour, day of month, month, day of week. `30 2 * * 0` means 02:30 every Sunday.

## Manage schedules

- **Pause / resume**: a paused schedule keeps its settings but never fires.
- **Edit**: loads the schedule into the editor.
- **Run now**: runs it immediately without waiting.
- **Delete**: removes the schedule; past job runs are kept.
- The table shows **Last run** and **Next run**. The scheduler checks every 60 seconds.

> **Tip:** Spread schedules for many sites over the night (03:00, 03:20, 03:40...). At most 3 jobs run at once by
> default (see [Resources and performance](resources-and-performance)), the rest wait in the queue.

## How jobs behave

- Two jobs that change the same site never run at the same time, and identical queued jobs are merged.
- Time limits: crawl and plan 2 h, inject 1 h, link health 3 h, audit 6 h, other jobs 30 min.
- A site can have at most 100 queued jobs.
- If SiloForge was stopped mid-job, the job is re-queued or marked failed at the next start.
- Every job has a log: click it in **Jobs** or **All jobs**. You can cancel queued and running jobs there.

## Search Console

Connect Google Search Console for a site (a service-account JSON, stored encrypted) to add ranking data to scoring:
pages in positions 4-10 count as "striking distance" and get a boost. Every audit pulls fresh data. Without it the
`gsc` signal is simply switched off.
