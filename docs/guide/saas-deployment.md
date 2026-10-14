---
title: SaaS deployment
section: Accounts & SaaS
order: 5
---

# SaaS deployment

By default SiloForge listens only on this PC (`127.0.0.1`) and nobody else can reach it. To offer it to other people
over the internet you put it behind a **reverse proxy with HTTPS**. This page is the short version; the full runbook
with config examples is in `docs/OPERATIONS.md` ("SaaS deployment").

> **Never expose SiloForge over plain HTTP.** Passwords, session tokens and API tokens would travel in clear text.
> Only change the listen address to `0.0.0.0` when a TLS proxy is in front and the port itself is firewalled.

## Steps

1. **Pick an address**, for example `https://links.example.com`, and point its DNS at your server.
2. **Put a TLS proxy in front.** Two easy options:
   - **Caddy** on the same machine: it gets and renews certificates by itself and forwards to `127.0.0.1:8600`.
   - **Cloudflare Tunnel** (`cloudflared`): no open ports at all; the tunnel forwards to `http://127.0.0.1:8600`.
   SiloForge itself can stay on `127.0.0.1` in both cases.
3. **Settings → Platform**:
   - **Public URL**: `https://links.example.com`. Every link in an email (verification, reset, invitation) uses it.
   - **Trust proxy**: on, so SiloForge reads the real client IP and `https` from the proxy headers. Turn it on only
     when a proxy is actually in front; otherwise clients could fake their IP.
4. **Mail**: set up SMTP in **Admin → Email** and send a test email. Without it, copy links by hand from the outbox.
5. **Registration**: choose **Closed**, **Invite only** or **Open** in **Admin → Registration**, and turn on
   **Require email verification** for open sign-up.
6. **Branding**: app name, logo, support email, terms and privacy links.
7. **Plans**: check the limits in **Admin → Plans** and the default plan for sign-ups.

## Security checklist

- [ ] HTTPS only, through Caddy or Cloudflare Tunnel; SiloForge's port is not reachable from the internet.
- [ ] Registration is **Closed** until you are ready, then **Invite only** or **Open** with email verification.
- [ ] Every platform admin has **2FA** turned on and has saved their recovery codes.
- [ ] SMTP works (test email sent), or you check the outbox regularly.
- [ ] Backups run and are verified (see [Backups and migration](backups-and-migration)); keep `secrets.env` safe and
      out of shared folders, because it holds the keys that protect passwords and stored secrets.
- [ ] You review **Admin → Audit** from time to time, and disable accounts you don't recognise.
- [ ] Sign-in and sign-up rate limits stay on (they are on by default).

## Single-user installs

If you only use SiloForge yourself on this PC, you don't need any of this. Keep the default loopback address and
closed registration.
