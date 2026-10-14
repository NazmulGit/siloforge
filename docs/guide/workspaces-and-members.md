---
title: Workspaces and members
section: Accounts & SaaS
order: 2
---

# Workspaces and members

A **workspace** is a shared space for a team or a client. It owns sites, API tokens, the MCP policy and a
[plan](plans-and-usage). Everyone who works on those sites is a **member** of the workspace with a **role**. You
can belong to several workspaces.

A single-user install has one workspace, **Default**, on the Unlimited plan, and every site you had before is in
it. You never have to touch workspaces unless you want to share SiloForge with other people.

## Switch workspace

The **workspace switcher** in the top bar shows the current workspace. Pick another one to see its sites, jobs and
usage. New sites, tokens and schedules are created in the current workspace.

## Roles

| Role | Can |
|---|---|
| **Viewer** | Read everything in the workspace: sites, suggestions, reports, jobs, logs. Changes nothing. |
| **Member** | Everything a viewer can, plus crawl, plan, approve, edit anchors, inject, undo, Link Runs and schedules. |
| **Admin** | Everything a member can, plus add sites, edit site settings, rotate site keys, connect Search Console, change Auto-Linker settings, manage members and invites, workspace API tokens and MCP policy, rename the workspace. |
| **Owner** | Everything an admin can, plus change other admins' roles, transfer ownership and delete the workspace. Each workspace has exactly one owner. |

**Platform admins** run the whole SiloForge installation. This is an account type, not a workspace role:

| Area | What a platform admin can do |
|---|---|
| Sites and workspaces | Open every workspace and site, even without being a member |
| [Admin console](admin-console) | Users, workspaces, plans, invites, email, registration, branding, audit log |
| Plans | Assign plans to workspaces and edit plan limits |
| Support | **View as workspace**: see a workspace exactly as its members do, read-only |

A site in a workspace you don't belong to simply does not exist for you: it answers "not found".

## Create a workspace

Use **+ New workspace** in the workspace switcher and enter a name (and optionally a short slug). Platform admins
can always create workspaces. Other users can create up to 3 of their own while registration is open or invite-only.
New workspaces start on the platform's default plan (usually Free).

## Invite people

You need the **admin** role in the workspace.

1. Go to **Workspace → Members & invites** and click **Invite**.
2. Enter the person's email (for example `editor@example.com`) and pick a role: viewer, member or admin.
3. SiloForge emails an invitation link that is valid for **7 days**. When the platform has no mail server, the link
   is shown to you instead: copy it and send it yourself.
4. The person opens the link, signs in (or registers) with that same email and clicks **Accept**.

Pending invites count towards the plan's member limit. **Cancel** an invite you no longer need.

## Manage members

In **Workspace → Members & invites**:

- **Change role**: admins can switch members between viewer, member and admin. Only the owner can change the role
  of another admin.
- **Remove**: admins can remove members, and anyone can leave a workspace themselves. The owner cannot be removed;
  transfer ownership first.

## Workspace settings

**Workspace → Settings** (admins and the owner):

- **Profile**: name and slug.
- **Plan & usage**: the plan, its limits and your usage (see [Plans and usage](plans-and-usage)).
- **API tokens and MCP policy**: tokens created here belong to this workspace, and the MCP tool policy applies to
  agents that use them (see [API, MCP and AI agents](api-and-mcp)).
- **Danger zone** (owner only):
  - **Transfer ownership** to another member. You stay in the workspace as an admin.
  - **Delete workspace**: type the slug to confirm. The workspace and its sites disappear at once and are purged
    after **30 days**; until then a platform admin can restore it. The Default workspace cannot be deleted.

## Suspended workspaces

A platform admin can suspend a workspace. Members can still read everything, but every change answers
"workspace suspended" until the workspace is resumed.

## View as workspace (platform admins)

For support, a platform admin can open **Admin → Workspaces → View as**. SiloForge then shows only that workspace's
sites and data, exactly as its members see them. It is strictly read-only: every change is refused ("view-as is
read-only"), and each view-as request is recorded in the audit log.
