# The GPU session, 2026-04-06

## What these files are, and what they are not

These are **console transcripts**, copied out of the terminal that ran them. They
are not the harness's own run directories.

Every other directory under `results/` holds what `warmpath-bench` wrote:
`report.json` with the config, the seed, the git SHA and the validity verdict,
`percentiles.csv`, and the per-request records. Those files were written here too,
onto the rented machine, and the machine was deleted before anyone copied them
off. That was a mistake and it is recorded in the retrospective.

So the numbers in `RESULTS.md` under "Against real vLLM" are backed by what you
see here and no more. You can read what the harness printed. You cannot re-derive
a percentile from the raw records, because the raw records are gone.

Everything else this project publishes is backed by real run directories. This
session is the exception, and it is labelled rather than quietly mixed in.

## What is missing that the other directories have

- Per-request records, so no percentile can be recomputed.
- `report.json`, so the exact git SHA of each run is not recorded. The machine
  had the repository checked out at `main`. The last overhead run, and only that
  one, ran with `TCP_NODELAY` set on both sockets. The runs before it did not.
  That is reconstructed from what was done, not read from a file.
- Machine-readable anything. These are text dumps.

## The hardware

One NVIDIA L4 on a GCE `g2-standard-8` in us-central1-a, 8 vCPU and 32 GB. Image
`pytorch-2-9-cu129-ubuntu-2204-nvidia-580`. Driver 580.173.02.

Two vLLM 0.27.1 servers shared the one GPU, because the GPU quota request came
back approved for one device rather than the four asked for. Each server was
capped at 112 KV blocks to reproduce the cache scarcity the mock runs use.

That arrangement makes cache behaviour measurable and latency not. Two engines
taking turns on one device contend by more than routing can save. Both halves are
here, and the latency half is reported as unusable rather than left out.

## The files

| file | what ran |
|---|---|
| `00-environment.txt` | `nvidia-smi`, and the vLLM startup lines that set the cache size |
| `01-hit-rate-validation.txt` | one bench run, then `validate-hit-rate.sh` |
| `02-policy-matrix.txt` | round-robin against prefix-affinity-balanced, 3 runs each |
| `03-overhead-before-nodelay.txt` | router overhead, first attempt, showing the stall |
| `04-overhead-fix-not-deployed.txt` | second attempt, which tested the same binary again |
| `05-overhead-after-nodelay.txt` | third attempt, with the fix actually built |
| `06-single-request-cache-check.txt` | the same prompt sent three times to one idle server |
| `summary.json` | the headline numbers, which `scripts/plot.py` reads to draw the chart |
