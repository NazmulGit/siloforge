---
title: Inject and undo
section: Linking
order: 5
---

# Inject and undo

Injecting writes approved links into your live WordPress posts. It is the only step, apart from undo, that changes
your site.

## Before you inject

- The site has WordPress credentials (see [Sites](sites)). Without them, approved links stay queued for the
  [plugin](wordpress-plugin).
- **Tools → Bulk → Insertion settings** are as you want:
  - **Mode**: `Pipeline` tries native → rephrase → context in that order; `Single: native` (and so on) uses only one
    method. Rephrase and context need an AI model.
  - **Preset**: `conservative` (fewer, safer links: higher score needed), `balanced`, `aggressive` (more links, lower
    bar). Score thresholds are native ≥ 60, rephrase ≥ 40, context ≥ 30 (out of 100), raised or lowered by 10 for
    conservative or aggressive.

## Inject

1. Open **Tools → Bulk**. It lists the approved links ready to inject.
2. Click **Run Bulk Inject** (or finish the Bulk wizard with **Start Linking**).
3. Watch the inject job under **Jobs**. The result counts:

| Count | Meaning |
|---|---|
| **applied** | Written and verified on the live page. |
| **marked** | Not written. Either waiting for the plugin or an AI (`content_gap`), or the anchor no longer exists because the post was edited. The row says which. |
| **failed** | The write or the check failed. The post was restored automatically. |

Only **applied** links are live. Real run: 11 links injected and verified in under 5 seconds on a local site.

> **Warning:** Do not stop SiloForge while an inject job runs. Check **All jobs** first.

## What keeps your content safe

1. **Fresh raw content.** SiloForge reads the post again just before writing, in raw form, so blocks and HTML survive.
2. **Exact position.** The link is spliced into the exact paragraph, checked by a hash. If that paragraph changed, the
   link is not written.
3. **Your edits win.** The update is sent with a "last modified" precondition. If someone edited the post in
   WordPress in the meantime, SiloForge does not overwrite it; it retries on the new content or leaves the link
   approved with a reason.
4. **Snapshot.** The post is snapshotted before every write.
5. **Live check.** SiloForge loads the live page (bypassing caches where it can) and checks that the link is there,
   once, and that no text or tags were lost. If the check fails, the post is rolled back and the link is `failed`.
   If the page cannot be checked (cache, timeout), the link stays applied with a note.

Gutenberg block counts stay unchanged, and no tracking parameters are added to links.

> **Warning:** **Settings → Linking Rules → Injection safety → Skip post-injection verify** turns off step 5. Leave it
> off: it is the only proof that a link actually landed.

## Undo

1. Open **Tools → Bulk**.
2. Choose **Undo All**, or select links and undo only those. (History shows every applied link.)
3. The undo job removes exactly the inserted `<a>` tag. Any other edits made since stay.

In testing, undo returned the posts byte-for-byte to their state before the inject. Undo only touches links that
SiloForge injected, never links that were already in your content. Reverted links can be approved and injected again.

## Cancel

Cancel a running inject in **All jobs**. It stops at the next checkpoint. Links already applied stay applied; undo them
if needed.
