---
title: Accounts and sign-in
section: Accounts & SaaS
order: 1
---

# Accounts and sign-in

From 2.4 SiloForge can serve more than one person: each person has their own account, works in one or more
[workspaces](workspaces-and-members), and can protect the account with two-factor authentication (2FA).

> **Single-user install?** Nothing changes for you. Your admin account, your sites, your API tokens and your Hermes
> MCP config keep working. All your sites live in the **Default** workspace on the **Unlimited** plan, and
> registration stays closed unless you open it.

## Sign in

1. Open the SiloForge address: for a local install `http://127.0.0.1:8600/`, for a hosted one the address your admin
   gave you (for example `https://links.example.com/`).
2. Enter your email and password. Use the eye icon to show or hide the password while you type.
3. Tick **Remember me** to stay signed in for 30 days on this browser. Untick it on a shared computer: the session
   then ends after 1 day.
4. If 2FA is on for your account, enter the 6-digit code from your authenticator app (or a recovery code).

What the messages mean:

| Message | Meaning |
|---|---|
| Wrong email or password | Check both. After 5 failed tries the account is locked for 30 seconds, then longer (up to 15 minutes). |
| Too many attempts / locked | Wait a moment and try again. Ask your admin if you still can't get in. |
| Account disabled | A platform admin has disabled the account. Contact them (the support email is shown on the sign-in page when set). |

Links to **Create an account** and **Forgot password?** appear only when the platform allows them. The sign-in page
can also show the platform's name, logo and links to its terms and privacy pages.

## Create an account

Whether you can register depends on the platform's registration mode:

| Mode | What happens |
|---|---|
| **Closed** (default) | No sign-up. Only a platform admin can create accounts (**Admin → Users**). The very first account on a fresh install can still be created from the PC itself. |
| **Invite only** | You need an invitation link from a workspace admin. You join that workspace. |
| **Open** | Anyone can sign up. You get your own new workspace, usually on the **Free** plan. |

To register, open **Create an account** and enter your name, email and a password, plus (in open mode) a name for
your workspace. If you came from an invitation link, you join the inviting workspace instead. The platform may only
accept some email domains, and limits how many accounts can be created per hour from one address.

## Verify your email

When the platform requires email verification, SiloForge sends a link to your address after sign-up (valid for
48 hours). You can sign in before you click it, but you cannot add sites until the address is verified.

Didn't get the mail? Use **Resend verification email** in **Settings → Account**, and check your spam folder. If the
platform has no mail server, ask a platform admin: they can see the link in the admin console and send it to you, or
mark your address as verified.

## Forgot your password

1. On the sign-in page click **Forgot password?** and enter your email. SiloForge always answers "check your inbox",
   whether the address has an account or not.
2. Open the link in the email. It works **once** and for **1 hour**.
3. Choose a new password. Every session of your account is signed out, so sign in again on your other devices.

API tokens are not affected by a password reset, so your agents keep working.

## Change your email address

In **Settings → Account → Email**, enter the new address and your current password. SiloForge sends a confirmation
link to the **new** address. Your address changes only when you click it.

## Two-factor authentication (2FA)

With 2FA, signing in also needs a 6-digit code from an authenticator app (for example Microsoft Authenticator,
Google Authenticator, Aegis or a password manager with TOTP support). Platform admins should always turn it on.

**Turn it on**

1. Go to **Settings → Account → Two-factor authentication** and click **Set up**. Enter your password.
2. Scan the QR code with your authenticator app. If you can't scan it, type the secret shown under the QR code.
3. Enter the 6-digit code the app shows and click **Enable**.
4. SiloForge shows **10 recovery codes** (like `abcde-12345`). Save them somewhere safe, for example in your password
   manager. Each one works once, and they are shown only this one time.

**Signing in with 2FA**: after your password, enter the current code from the app. Each code works only once, so if
a code is refused, wait for the next one. Lost your phone? Enter a recovery code instead. **Settings → Account**
shows how many recovery codes you have left.

**New recovery codes**: **Generate new codes** (needs your password) replaces all the old ones.

**Turn it off**: **Disable** needs your password and a current code. If you have lost both the app and your recovery
codes, ask a platform admin to reset 2FA for your account.

> **Note:** The old v1 sign-in API (`/api/v1/auth/login`) cannot ask for a code, so accounts with 2FA must sign in
> through the web app. Scripts and agents should use [API tokens](api-and-mcp) anyway.

## Sessions

**Settings → Account → Sessions** lists every browser that is signed in to your account: when it signed in, when it
was last seen, its IP address and browser, and whether **Remember me** was used. The current one is marked.

- **Revoke** ends one session, for example a browser you left signed in at work.
- **Sign out everywhere** ends all of them. You can choose to revoke your API tokens too.
- **Sign out** in the user menu ends only the current session.

Changing or resetting your password also signs out every session.

Next: [Workspaces and members](workspaces-and-members).
