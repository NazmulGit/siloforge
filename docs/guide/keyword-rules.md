---
title: Keyword rules
section: Linking
order: 6
---

# Keyword rules

Keyword rules link a fixed phrase to a fixed page wherever the phrase appears. Use them for terms you always want to
point to one page, such as a product category or a glossary entry.

## Add a rule

1. Open **Tools → Keyword Linker**.
2. Enter:
   - **Keyword**: the phrase, for example `water bottle`.
   - **Target URL**: the exact URL of the target post, for example `https://example.com/best-water-bottles/`.
   - **Anchor type**: usually `exact` or `partial`.
   - **Max links**: how many source posts may get this link (1-100).
3. Save the rule. Rules can be paused, edited or deleted later.

## Run the rules

1. Click **Run** in **Tools → Keyword Linker**.
2. SiloForge finds published posts that contain the keyword as a whole phrase (not inside another word) and creates
   suggestions with the keyword as anchor. Each source post gets at most the site's maximum links per post.
3. Review and approve them like any other suggestion. Keyword links **still need approval** and an inject.

> **Tip:** The target URL must match the crawled URL exactly, including the trailing slash. Copy it from Reports.

## Ignore keywords

**Tools → Ignore Keywords** is a stop list. Any anchor that contains one of these phrases is never used, by plans or by
keyword rules. Add your brand names, competitor names, legal phrases ("terms of service") and anything sensitive.
