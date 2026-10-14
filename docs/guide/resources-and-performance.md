---
title: Resources and performance
section: Administration
order: 1
---

# Resources and performance

SiloForge is built to share the PC with other work. **Settings → Resources & Performance** (admin) controls how much CPU,
memory and GPU it may use. Most changes apply at once; a few need a restart (see the end of this page).

## CPU and memory

| Setting | Default | Range | What it does |
|---|---|---|---|
| **CPU threads** | 0 = a quarter of your logical cores | 0 - all cores | Threads for scoring, anchors and CPU embedding. The page shows the effective number. *Restart needed.* |
| **Max concurrent jobs** | 3 | 1-16 | Jobs on different sites that run at once. Use 1 on a small PC. |
| **Vector cache (MB)** | 512 | 64-32768 | RAM for cached post vectors. About 1.6 KB per paragraph: a 112,000-paragraph site needs about 270 MB. Unused sites are dropped after 10 minutes. |
| **Max memory (MB)** | 0 = off | 0 or 256 - total RAM | Soft cap. When SiloForge goes above it, the vector cache shrinks and heavy jobs wait. |
| **Low-priority jobs** | on | on/off | Heavy work runs at below-normal Windows priority, so your other programs stay responsive. Off = about 20-25 % faster CPU embedding on a dedicated PC. |
| **Defer when CPU above (%)** | 0 = off | 0-100 | Heavy jobs wait while the whole PC is busier than this. |

Measured on a 12-core/24-thread PC with CPU embedding: 2 threads → 134 texts/s using 1.8 cores; 6 threads → 204 texts/s
using 3.8 cores; 12 threads → 221 texts/s using 7 cores. Doubling from 6 to 12 threads costs 75 % more CPU for 8 % more
speed, so the default is a good balance.

At idle SiloForge uses about 26 MB of RAM; during a big plan 0.3-0.45 GB, and up to about 0.7 GB while embedding.

## CPU / GPU balancing (embeddings)

Embedding text is the heaviest task. Choose where it runs with **Embedding mode**:

| Mode | What happens | Choose it when |
|---|---|---|
| `auto` (default) | GPU if one is available, else CPU; a remote provider as fallback | Almost always |
| `gpu` | GPU only (DirectML: AMD, NVIDIA or Intel on Windows) | You want the CPU left free |
| `cpu` | CPU only | No usable GPU, or the GPU is busy with other work |
| `hybrid` | GPU **and** CPU work on the same job in parallel; the GPU takes about the **GPU share** of the batches (default 0.7, range 0.1-0.9) and the CPU takes the rest | Large first-time embeddings, when you want maximum speed |
| `remote` | An OpenRouter-style embedding provider first, local as fallback | A PC without a GPU and with a slow CPU |

Other compute settings: **Batch size** (default 64, range 8-512), the **embedding chain** order, the remote model, and
up to two remote API keys (write-only, stored in `secrets.env`). The page shows the detected GPU and the **active
device** (for example `directml` or `cpu`).

Measured speeds: GPU (DirectML, a mid-range Radeon) 387-715 texts/s using under one CPU core; CPU with 6 threads about
204 texts/s. A full first plan of a 2,600-post site (115,000 texts) took about 3 minutes on the GPU.

> **Tip:** If the GPU fails to start (old driver), SiloForge falls back to the CPU on its own. Update the graphics
> driver and check **System** for the error.

## Other sections on the same page

- **Crawl**: parallel requests per crawl (default 4, range 1-32), timeout, max pages (20,000), allow private hosts.
- **Monitor**: sampling interval (5 s), history kept (14 days), alert thresholds (CPU 95 %, memory 92 %, free disk
  2,048 MB), log retention.

## Restart

Some keys (CPU threads, server addresses) only take effect after a restart. The page then shows **Restart required**
with the list of keys. Click **Restart**: SiloForge relaunches itself and is back within seconds.

> **Warning:** Do not restart while an inject job is running. Check **All jobs** first.

## Watching resource use

- **System** (admin): CPU, memory and disk over 1 hour, 24 hours or 7 days, the job queue, and recent errors.
- **All jobs**: per job wall time, CPU seconds and peak memory. When several jobs ran at once these figures are shared.
