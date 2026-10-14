---
title: Admin console
section: Accounts & SaaS
order: 4
---

# Admin console

The **Admin** section is for **platform admins**: the people who run the SiloForge installation. Other users don't
see it. Everything done here is recorded in the audit log.

## Overview

**Admin → Overview** shows the whole platform at a glance: users (total, admins, disabled, unverified, with 2FA),
workspaces (total, suspended, deleted), sites, posts, Link Runs today, AI tokens this month, open errors, the mail
status (configured, messages waiting in the outbox), the registration mode and system health (version, uptime,
running and queued jobs).

## Users

**Admin → Users** lists every account with its workspaces, 2FA status, last sign-in and active sessions. Search by
name or email.

| Action | What it does |
|---|---|
| **Create user** | Email, name, role and optionally a workspace and role there. Leave the password empty to send a "set your password" link instead. |
| **Edit** | Name, email, role (user or platform admin). |
| **Send reset link** | Emails a password reset link (or shows it in **Email** when there is no mail server). |
| **Force logout** | Ends every session of the user. |
| **Disable / Enable** | A disabled user cannot sign in ("account disabled"). |
| **Verify email** | Marks the address as verified by hand. |
| **Reset 2FA** | Turns two-factor authentication off, for a user who lost their phone and recovery codes. |
| **Delete** | Removes the user. Their workspaces and sites move to another user you pick (default: you). You cannot delete yourself or the last platform admin. |

## Workspaces

**Admin → Workspaces** lists every workspace with its owner, plan, members, sites and usage against the limits.
Filter by status (active, suspended, deleted).

- **Change plan**: assigns a plan; it applies immediately. This is how upgrades work in 2.4.
- **Suspend / Resume**: a suspended workspace stays readable, but every change is refused.
- **Delete**: soft delete; the workspace is purged after 30 days. **Restore** brings it back before then.
- **View as**: read-only support view of the workspace (see [Workspaces and members](workspaces-and-members)).

## Plans

**Admin → Plans**: create, edit and delete plans. Each plan has a name, a price label (text only, for example
"$29 / month"), whether it is public, a sort order and its limits. Leave a number empty for "unlimited". A plan that
is still used by a workspace cannot be deleted. See [Plans and usage](plans-and-usage) for the default plans.

## Invites

**Admin → Invites** lists the open invitations of every workspace.

## Email

**Admin → Email** holds the mail server (SMTP) settings: host, port, TLS mode (STARTTLS, TLS or none), user name,
password (never shown again after saving), sender address and name.

1. Fill in the settings of your mail provider and save.
2. Click **Send test email** and enter your own address (for example `admin@example.com`). The result says whether
   it was delivered or shows the error.

Without a mail server SiloForge still works: every mail it would send (verification, password reset, invitation,
set-password links) is kept in the **outbox** below with its link. Copy the link and send it to the person yourself,
then delete the outbox entry.

## Registration

| Setting | Meaning |
|---|---|
| **Mode** | **Closed** (default): only admins create accounts. **Invite only**: sign-up needs an invitation. **Open**: anyone can sign up and gets a new workspace. |
| **Require email verification** | Unverified users can sign in but cannot add sites. |
| **Allowed / denied domains** | Only accept, or refuse, sign-ups from these email domains (for example `example.com`). |
| **Rate limit per hour** | Sign-ups per IP address per hour (default 5). |
| **Default plan** | Plan of workspaces created by sign-up (default Free). |

## Branding

App name, logo URL, primary colour, support email and links to your terms and privacy pages. They are shown on the
sign-in and registration pages.

## Platform

**Settings → Platform** (or **Admin → Platform**) also holds the deployment settings: the **public URL** used in
every emailed link, and **trust proxy** for installs behind a reverse proxy. See [SaaS deployment](saas-deployment).

## Audit

**Admin → Audit** is the platform-wide activity log with the same filters as [Logs](logs): sign-ins, sign-ups,
member and plan changes, admin actions and every view-as request.
