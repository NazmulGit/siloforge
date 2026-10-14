---
title: Link health
section: Analysis
order: 2
---

# Link health

**Link Health** checks every link in your published content: its status, redirect chain and `rel` attributes. It only
reads; it changes nothing.

## Run a check

1. Open the site's **Link Health** page.
2. Click **Run link health check**. This checks internal and external links.
3. Results appear when the job finishes. **Export CSV** downloads every checked link.

For a faster routine check of internal links only, use a `link_health` schedule with the payload
`{"internal_only": true}` (see [Schedules and audits](schedules-and-audits)). External links are then listed but not
requested.

Large sites can have many thousands of links; a check may run for a while (the job limit is 3 hours). Requests are
throttled per host and slow down automatically when a server answers 429 or 503.

## The four tabs

| Tab | Shows | Fix |
|---|---|---|
| **Broken** | 404, 410, DNS failures and redirect loops | Update or remove the link in WordPress. |
| **Redirects** | Links that go through one or more redirects, with the hop chain | Point the link straight at the final URL. |
| **Nofollow** | Links with `rel=nofollow`, `sponsored` or `ugc`; internal ones are highlighted | On your own site, internal nofollow is almost always unintended. |
| **Breadcrumbs** | Breadcrumb coverage: the visible trail and the JSON-LD `BreadcrumbList` | Fix pages where only one of the two is present. |

## Verdicts

| Verdict | Meaning |
|---|---|
| broken | Not found (404), gone (410), DNS failure, redirect loop |
| unverified | Timeout, rate limited (429), connection error, blocked (401/403), too many hops. Check again later; often a server protecting itself. |
| ok | Reached the destination |

> **Tip:** Schedule a weekly internal-only check, or use an audit schedule, which includes a link-health check
> (see [Schedules and audits](schedules-and-audits)).

Real example: on a 360-post site with about 12,500 links, the internal-only check requested 178 internal links and
found 2 real 404s and 2 redirects; the 12,343 external links were listed without being requested.
