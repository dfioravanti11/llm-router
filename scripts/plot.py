#!/usr/bin/env python3
"""Regenerate every chart under docs/charts from the committed benchmark data.

Run it with no arguments and no network:

    python3 scripts/plot.py

Everything here reads percentiles.csv, report.json, campaign.json and the
worker-stats jsonl files. The per-request records.jsonl files are deliberately
untouched. They are large, and on this machine they live behind a cloud sync
that takes minutes to fault them in, so a script that reads them is a script
nobody runs twice.

Four rules come from the project spec and they shape almost every choice below.
No bar chart of means, ever. No single run presented as the result. The tail
has to be legible, which is why the vertical axis is 1/(1-p) on a log scale
rather than a plain cumulative fraction. And an interval wide enough to contain
zero has to look like one.
"""

import csv
import json
import math
import os
import sys

import matplotlib

matplotlib.use("Agg")

import matplotlib.pyplot as plt
import numpy as np
from matplotlib.lines import Line2D
from matplotlib.ticker import FixedLocator, FuncFormatter, NullLocator

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RESULTS = os.path.join(ROOT, "results")
CHARTS = os.path.join(ROOT, "docs", "charts")

# The harness records these seven quantiles and no others, so every tail plot
# is seven points joined up. They land almost evenly on a log 1/(1-p) axis,
# which is a happy accident of how the harness was configured.
PCTS = (50.0, 75.0, 90.0, 95.0, 99.0, 99.9, 99.99)

DPI = 140
METRIC = "ttft_from_intended"


class MissingData(Exception):
    """Raised when an input file or field the chart needs is not on disk."""


# ---------------------------------------------------------------------------
# palette
# ---------------------------------------------------------------------------

# Okabe-Ito, which is safe for the common colour vision deficiencies. Each
# series also gets its own dash pattern and its own marker, because the README
# may well be read on a printout or by someone who sees no colour at all. Line
# shape carries the identity and colour only reinforces it.
STYLE = {
    "round-robin": dict(color="#0072B2", ls=(0, (5, 1.6)), marker="o"),
    # Reddish purple rather than the lighter sky blue, which turned almost
    # white once the chart was converted to greyscale.
    "least-loaded": dict(color="#CC79A7", ls=(0, (1, 1.6)), marker="s"),
    "power-of-two": dict(color="#009E73", ls=(0, (4, 1.3, 1, 1.3)), marker="^"),
    "prefix-affinity": dict(color="#D55E00", ls=(0, (6, 1.4, 1, 1.4, 1, 1.4)), marker="D"),
    "prefix-affinity-balanced": dict(color="#111111", ls="-", marker="v"),
    # The overhead experiment puts direct alongside round-robin, so direct
    # needs a shape and a colour that round-robin is not already using.
    "direct": dict(color="#E69F00", ls=(0, (1, 1.5)), marker="s"),
    "open loop": dict(color="#111111", ls="-", marker="v"),
    "closed loop": dict(color="#D55E00", ls=(0, (6, 1.4, 1, 1.4)), marker="D"),
}

GREY = "#6b6b6b"
FAINT = "#c9c9c9"

plt.rcParams.update(
    {
        "figure.facecolor": "white",
        "axes.facecolor": "white",
        "savefig.facecolor": "white",
        "font.size": 9,
        "axes.titlesize": 10,
        "axes.labelsize": 9,
        "legend.fontsize": 8,
        "xtick.labelsize": 8,
        "ytick.labelsize": 8,
        "axes.edgecolor": "#444444",
        "axes.linewidth": 0.8,
        "grid.color": "#dddddd",
        "grid.linewidth": 0.6,
        "lines.solid_capstyle": "round",
        "lines.dash_capstyle": "round",
    }
)


# ---------------------------------------------------------------------------
# loading
# ---------------------------------------------------------------------------


def require(path):
    if not os.path.exists(path):
        raise MissingData("missing input: %s" % path)
    return path


def run_dirs(arm_dir):
    """Every run directory under one arm, oldest first.

    Run ids start with a unix millisecond timestamp, so a plain sort puts them
    in the order they were executed. The worker-stats files are numbered from
    one in that same order, and pairing them up depends on it.
    """
    require(arm_dir)
    names = sorted(
        d
        for d in os.listdir(arm_dir)
        if os.path.isdir(os.path.join(arm_dir, d))
    )
    if not names:
        raise MissingData("no run directories under %s" % arm_dir)
    return [os.path.join(arm_dir, d) for d in names]


def read_percentiles(run_dir, metric=METRIC):
    """Latency in milliseconds at each recorded quantile, for one run."""
    path = require(os.path.join(run_dir, "percentiles.csv"))
    out = {}
    with open(path, newline="") as fh:
        for row in csv.DictReader(fh):
            if row["metric"] == metric:
                out[float(row["percentile"])] = int(row["value_us"]) / 1000.0
    missing = [p for p in PCTS if p not in out]
    if missing:
        raise MissingData("%s has no %s at percentiles %s" % (path, metric, missing))
    return out


def read_report(run_dir):
    with open(require(os.path.join(run_dir, "report.json"))) as fh:
        return json.load(fh)


def read_campaign(arm_dir):
    with open(require(os.path.join(arm_dir, "campaign.json"))) as fh:
        return json.load(fh)


def campaign_metric(arm_dir, key):
    """One aggregated metric, with the point estimate and interval RESULTS uses.

    RESULTS.md quotes the median across runs as the number and mean +/- the
    half width as the interval. That is a slightly odd pairing, and the charts
    copy it exactly so a reader can put chart and table side by side without
    finding two different answers.
    """
    camp = read_campaign(arm_dir)
    metrics = camp.get("metrics", {})
    if key not in metrics:
        raise MissingData("%s/campaign.json has no metric %s" % (arm_dir, key))
    m = metrics[key]
    return dict(
        median=m["median"],
        mean=m["mean"],
        half=m["ci95_half_width"],
        values=list(m["values"]),
    )


def read_worker_stats(arm_dir):
    """Per-worker end-of-run counters, one list of workers per run.

    Each worker-stats-N.jsonl holds one json object per worker for run N. The
    counters are cumulative over the whole run, warmup included.
    """
    require(arm_dir)
    names = sorted(
        f
        for f in os.listdir(arm_dir)
        if f.startswith("worker-stats-") and f.endswith(".jsonl")
    )
    if not names:
        raise MissingData("no worker-stats-*.jsonl under %s" % arm_dir)
    runs = []
    for name in names:
        with open(os.path.join(arm_dir, name)) as fh:
            workers = [json.loads(line) for line in fh if line.strip()]
        if not workers:
            raise MissingData("%s/%s is empty" % (arm_dir, name))
        for w in workers:
            for field in ("started", "cache"):
                if field not in w:
                    raise MissingData("%s/%s has no %s field" % (arm_dir, name, field))
            for field in ("prefix_cache_queries", "prefix_cache_hits"):
                if field not in w["cache"]:
                    raise MissingData(
                        "%s/%s cache has no %s" % (arm_dir, name, field)
                    )
        runs.append(workers)
    return runs


def check_valid(run_dirs_list, label):
    """Refuse to draw a run the harness itself marked invalid.

    An invalid run is excluded from its campaign, so plotting it would put a
    line on the chart that contributes to no published number.
    """
    for d in run_dirs_list:
        rep = read_report(d)
        if not rep.get("validity", {}).get("valid", False):
            raise MissingData(
                "%s: run %s is marked invalid (%s)"
                % (label, os.path.basename(d), rep["validity"].get("reasons"))
            )


# ---------------------------------------------------------------------------
# the tail axis
# ---------------------------------------------------------------------------


def tail_y(p):
    return 1.0 / (1.0 - p / 100.0)


YVALS = [tail_y(p) for p in PCTS]


def _fmt_pct(value, _pos):
    p = 100.0 * (1.0 - 1.0 / value)
    if abs(p - round(p)) < 1e-6:
        return "p%d" % round(p)
    return ("p%g" % p)


def setup_tail_axis(ax, xlabel="time to first token (ms)"):
    """Percentile up the side on a log 1/(1-p) scale.

    A plain CDF spends nearly all its height on the boring middle and squeezes
    the worst one percent into the last pixel. This scale gives every decade of
    rarity the same room, so p99 and p99.9 are as readable as the median. It is
    the shape HdrHistogram plots use and the reason the project records exactly
    these quantiles.
    """
    ax.set_yscale("log")
    ax.yaxis.set_major_locator(FixedLocator(YVALS))
    ax.yaxis.set_major_formatter(FuncFormatter(_fmt_pct))
    ax.yaxis.set_minor_locator(NullLocator())
    ax.set_ylim(tail_y(50.0) / 1.12, tail_y(99.99) * 1.12)
    ax.set_xlabel(xlabel)
    ax.set_ylabel("percentile of requests")
    ax.grid(True, which="major", axis="both", alpha=0.7)
    ax.set_axisbelow(True)


def draw_arm(ax, name, runs_pcts, label=None, zorder=3):
    """One arm as thin per-run lines under a bold median line.

    Showing the runs is not decoration. On the even workload one prefix-affinity
    run has a p99 six times the other two, and any summary that hides that is
    hiding the finding.
    """
    style = STYLE[name]
    xs = np.array([[r[p] for p in PCTS] for r in runs_pcts], dtype=float)
    for row in xs:
        ax.plot(
            row,
            YVALS,
            color=style["color"],
            lw=0.8,
            alpha=0.30,
            solid_capstyle="round",
            zorder=zorder - 1,
        )
    med = np.median(xs, axis=0)
    ax.plot(
        med,
        YVALS,
        color=style["color"],
        ls=style["ls"],
        marker=style["marker"],
        markersize=3.6,
        markeredgewidth=0.0,
        lw=1.9 if name == "prefix-affinity-balanced" else 1.5,
        label=label or name,
        zorder=zorder,
    )
    return med


def titled(ax, title, subtitle=None):
    """Title with a grey standfirst underneath it.

    Matplotlib puts a title flush against the axes, so the pad has to make room
    for the standfirst by hand. It has to grow with the number of lines in it,
    since a fixed pad sized for one line lets a two line standfirst run into the
    title above it.
    """
    lines = subtitle.count("\n") + 1 if subtitle else 0
    ax.set_title(title, loc="left", pad=10 + 11 * lines if subtitle else 7)
    if subtitle:
        ax.text(
            0.0,
            1.015,
            subtitle,
            transform=ax.transAxes,
            fontsize=7.4,
            color=GREY,
            va="bottom",
            linespacing=1.35,
        )


def sorted_by_p99(arms):
    """Legend order that matches what the eye sees at the top of the chart."""
    return sorted(arms.items(), key=lambda kv: np.median([r[99.0] for r in kv[1]]))


def save(fig, name):
    os.makedirs(CHARTS, exist_ok=True)
    path = os.path.join(CHARTS, name)
    fig.savefig(
        path,
        dpi=DPI,
        bbox_inches="tight",
        pad_inches=0.12,
        pil_kwargs={"optimize": True},
    )
    plt.close(fig)
    size = os.path.getsize(path)
    print("  wrote %-34s %7.1f kB" % (name, size / 1024.0))
    return path


# ---------------------------------------------------------------------------
# chart 1: the headline
# ---------------------------------------------------------------------------

MATRIX_POLICIES = [
    "round-robin",
    "least-loaded",
    "power-of-two",
    "prefix-affinity",
    "prefix-affinity-balanced",
]


def load_matrix(workload):
    arms = {}
    for pol in MATRIX_POLICIES:
        arm = os.path.join(RESULTS, "policy-matrix", workload, pol)
        dirs = run_dirs(arm)
        check_valid(dirs, "%s/%s" % (workload, pol))
        arms[pol] = [read_percentiles(d) for d in dirs]
    return arms


def chart_even():
    arms = load_matrix("even")

    fig, ax = plt.subplots(figsize=(7.0, 4.6))
    setup_tail_axis(ax)
    for pol, runs in sorted_by_p99(arms):
        draw_arm(ax, pol, runs)

    ax.set_xscale("log")
    ax.set_xlim(10, 600)
    ax.xaxis.set_major_locator(FixedLocator([10, 20, 50, 100, 200, 500]))
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _p: "%g" % v))
    ax.xaxis.set_minor_locator(NullLocator())

    titled(
        ax,
        "Time to first token, even prefix popularity",
        "3 mock workers, 60 arrivals/s offered open loop, 3 runs per policy.\n"
        "Thin lines are single runs, bold is their median.",
    )
    # The interesting structure is a step, so name it. Cache-blind policies sit
    # near 12ms for half the requests and near 42ms for the rest, which is the
    # difference between a prefix that was already resident and one that had to
    # be prefilled from scratch.
    ax.annotate(
        "cache miss:\n~30 ms of prefill",
        xy=(43, tail_y(78.0)),
        xytext=(56, tail_y(58.0)),
        fontsize=7.2,
        color=GREY,
        ha="left",
        va="center",
        arrowprops=dict(arrowstyle="-", color=GREY, lw=0.7, shrinkA=2, shrinkB=2),
    )
    ax.legend(
        loc="lower right",
        frameon=True,
        framealpha=0.95,
        edgecolor="#cccccc",
        borderpad=0.5,
    )
    return save(fig, "even-ttft-tail.png")


# ---------------------------------------------------------------------------
# chart 2: the skewed workload, where naive affinity falls over
# ---------------------------------------------------------------------------


def hit_rate(workers):
    q = sum(w["cache"]["prefix_cache_queries"] for w in workers)
    h = sum(w["cache"]["prefix_cache_hits"] for w in workers)
    if q == 0:
        raise MissingData("worker stats report zero prefix cache queries")
    return 100.0 * h / q


def worker_shares(workers):
    """Share of started requests per worker, busiest first."""
    started = np.array([w["started"] for w in workers], dtype=float)
    if started.sum() == 0:
        raise MissingData("worker stats report zero started requests")
    return np.sort(100.0 * started / started.sum())[::-1]


def chart_skewed():
    arms = load_matrix("skewed")
    stats = {
        pol: read_worker_stats(os.path.join(RESULTS, "policy-matrix", "skewed", pol))
        for pol in MATRIX_POLICIES
    }
    for pol, runs in stats.items():
        if len(runs) != len(arms[pol]):
            raise MissingData(
                "skewed/%s has %d worker-stats files for %d runs"
                % (pol, len(runs), len(arms[pol]))
            )

    fig = plt.figure(figsize=(11.6, 6.2))
    gs = fig.add_gridspec(2, 2, width_ratios=[1.25, 1.0], hspace=0.52, wspace=0.28)
    ax_cdf = fig.add_subplot(gs[:, 0])
    ax_conc = fig.add_subplot(gs[0, 1])
    ax_trade = fig.add_subplot(gs[1, 1])

    # left: the same tail plot as the headline, on the workload where 80% of
    # requests carry one prefix.
    setup_tail_axis(ax_cdf)
    for pol, runs in sorted_by_p99(arms):
        draw_arm(ax_cdf, pol, runs)
    ax_cdf.set_xscale("log")
    ax_cdf.set_xlim(9, 4000)
    ax_cdf.xaxis.set_major_locator(FixedLocator([10, 30, 100, 300, 1000, 3000]))
    ax_cdf.xaxis.set_major_formatter(FuncFormatter(lambda v, _p: "%g" % v))
    ax_cdf.xaxis.set_minor_locator(NullLocator())
    titled(
        ax_cdf,
        "Time to first token, 80% of traffic on one prefix",
        "3 mock workers, 4 slots each, 60 arrivals/s, 3 runs per policy.\n"
        "Naive affinity sits right of every other policy at every percentile.",
    )
    ax_cdf.legend(loc="lower right", frameon=True, framealpha=0.95, edgecolor="#cccccc")

    # top right: where the requests actually went. One dot per worker per run,
    # so all three runs are on the page and nothing is averaged away.
    order = [p for p, _ in sorted_by_p99(arms)]
    for i, pol in enumerate(order):
        y0 = len(order) - 1 - i
        style = STYLE[pol]
        for j, workers in enumerate(stats[pol]):
            shares = worker_shares(workers)
            y = y0 + (j - (len(stats[pol]) - 1) / 2.0) * 0.19
            ax_conc.plot(
                shares,
                [y] * len(shares),
                ls="-",
                lw=0.7,
                color=FAINT,
                zorder=1,
            )
            ax_conc.scatter(
                shares,
                [y] * len(shares),
                s=22,
                facecolors=[style["color"]] + ["white"] * (len(shares) - 1),
                edgecolors=style["color"],
                linewidths=0.9,
                zorder=3,
            )
    ax_conc.axvline(100.0 / 3.0, color=GREY, ls=(0, (2, 2)), lw=0.9, zorder=0)
    ax_conc.text(
        100.0 / 3.0 + 1.5,
        len(order) - 0.35,
        "even split",
        fontsize=7,
        color=GREY,
        va="center",
    )
    ax_conc.set_yticks(range(len(order)))
    ax_conc.set_yticklabels(list(reversed(order)))
    ax_conc.set_ylim(-0.6, len(order) - 0.15)
    ax_conc.set_xlim(0, 92)
    ax_conc.set_xlabel("share of requests started, per worker (%)")
    titled(
        ax_conc,
        "Where the requests went",
        "One dot per worker per run, so all three runs are on the page.",
    )
    ax_conc.grid(True, axis="x", alpha=0.7)
    ax_conc.set_axisbelow(True)
    # Upper right is the only empty corner. The busiest prefix-affinity worker
    # lands near 80% on the bottom row, so a legend down there would cover the
    # single most important dot in the panel.
    ax_conc.legend(
        handles=[
            Line2D([], [], ls="", marker="o", mfc="#444444", mec="#444444",
                   markersize=5, label="busiest worker"),
            Line2D([], [], ls="", marker="o", mfc="white", mec="#444444",
                   markersize=5, label="the other two"),
        ],
        loc="upper right",
        frameon=True,
        framealpha=0.95,
        edgecolor="#cccccc",
        fontsize=7,
        borderpad=0.4,
    )

    # bottom right: the point of the whole figure. Hit rate is the metric a
    # cache-aware router is tempted to optimise, and here the policy that wins
    # it is thirty times slower at p99 than plain rotation.
    for pol in MATRIX_POLICIES:
        style = STYLE[pol]
        hs = [hit_rate(w) for w in stats[pol]]
        ps = [r[99.0] for r in arms[pol]]
        ax_trade.scatter(
            hs, ps, s=16, color=style["color"], alpha=0.45, linewidths=0, zorder=2
        )
        ax_trade.scatter(
            [np.median(hs)],
            [np.median(ps)],
            s=74,
            marker=style["marker"],
            color=style["color"],
            edgecolors="white",
            linewidths=0.8,
            zorder=4,
        )
    ax_trade.set_yscale("log")
    ax_trade.set_ylim(25, 5000)
    ax_trade.set_xlabel("prefix cache hit rate reported by the workers (%)")
    ax_trade.set_ylabel("p99 TTFT (ms)")
    titled(
        ax_trade,
        "Best hit rate, worst latency",
        "Small dots are single runs, large markers their median.",
    )
    ax_trade.grid(True, alpha=0.7)
    ax_trade.set_axisbelow(True)
    ax_trade.yaxis.set_major_locator(FixedLocator([30, 100, 300, 1000, 3000]))
    ax_trade.yaxis.set_major_formatter(FuncFormatter(lambda v, _p: "%g" % v))
    ax_trade.yaxis.set_minor_locator(NullLocator())
    pa_h = np.median([hit_rate(w) for w in stats["prefix-affinity"]])
    pa_p = np.median([r[99.0] for r in arms["prefix-affinity"]])
    ax_trade.annotate(
        "prefix-affinity",
        xy=(pa_h, pa_p),
        xytext=(-6, -16),
        textcoords="offset points",
        fontsize=7.5,
        color=STYLE["prefix-affinity"]["color"],
        ha="right",
    )
    rr_h = np.median([hit_rate(w) for w in stats["round-robin"]])
    rr_p = np.median([r[99.0] for r in arms["round-robin"]])
    ax_trade.annotate(
        "round-robin",
        xy=(rr_h, rr_p),
        xytext=(9, 6),
        textcoords="offset points",
        fontsize=7.5,
        color=STYLE["round-robin"]["color"],
        ha="left",
        va="bottom",
    )
    return save(fig, "skewed-affinity-hotspot.png")


# ---------------------------------------------------------------------------
# chart 3: coordinated omission
# ---------------------------------------------------------------------------


def chart_coordinated_omission():
    arms = {}
    dirs_by_arm = {}
    for key, folder in (("open loop", "open-loop"), ("closed loop", "closed-loop")):
        arm = os.path.join(RESULTS, "co-demo", folder)
        dirs = run_dirs(arm)
        check_valid(dirs, key)
        dirs_by_arm[key] = (arm, dirs)
        arms[key] = [read_percentiles(d) for d in dirs]

    fig, (ax_cdf, ax_tp) = plt.subplots(
        1, 2, figsize=(10.4, 4.4), gridspec_kw={"width_ratios": [1.35, 1.0], "wspace": 0.27}
    )

    setup_tail_axis(ax_cdf)
    for key in ("closed loop", "open loop"):
        draw_arm(ax_cdf, key, arms[key])
    ax_cdf.set_xscale("log")
    ax_cdf.set_xlim(9, 900)
    ax_cdf.xaxis.set_major_locator(FixedLocator([10, 20, 50, 100, 200, 500]))
    ax_cdf.xaxis.set_major_formatter(FuncFormatter(lambda v, _p: "%g" % v))
    ax_cdf.xaxis.set_minor_locator(NullLocator())
    titled(
        ax_cdf,
        "The same worker, measured two ways",
        "One mock worker with 2 serving slots, 3 runs per generator.\n"
        "Thin lines are single runs, bold is their median.",
    )
    ax_cdf.legend(loc="lower right", frameon=True, framealpha=0.95, edgecolor="#cccccc")

    # The closed-loop line is flat and short because its callers stop sending
    # whenever a response is slow. Say so on the chart, since the shape is the
    # argument.
    ax_cdf.annotate(
        "callers wait, so\narrivals thin out\nwhen it matters",
        xy=(np.median([r[99.9] for r in arms["closed loop"]]), tail_y(99.9)),
        xytext=(27, tail_y(99.5)),
        fontsize=7.5,
        color=STYLE["closed loop"]["color"],
        ha="left",
        va="center",
        arrowprops=dict(
            arrowstyle="-",
            color=STYLE["closed loop"]["color"],
            lw=0.7,
            shrinkA=3,
            shrinkB=3,
        ),
    )

    # Right: throughput against p99, one point per run. This is the part that
    # makes the result impossible to dismiss as a lighter load. The closed-loop
    # harness pushed more requests through the same worker and still reported a
    # far better tail.
    centre = {}
    for key in ("open loop", "closed loop"):
        arm, dirs = dirs_by_arm[key]
        style = STYLE[key]
        rates = [read_report(d)["counts"]["achieved_rate_per_second"] for d in dirs]
        p99s = [p[99.0] for p in arms[key]]
        centre[key] = (float(np.median(rates)), float(np.median(p99s)))
        ax_tp.scatter(
            rates, p99s, s=20, color=style["color"], alpha=0.45, linewidths=0, zorder=2
        )
        ax_tp.scatter(
            [np.median(rates)],
            [np.median(p99s)],
            s=90,
            marker=style["marker"],
            color=style["color"],
            edgecolors="white",
            linewidths=0.9,
            zorder=4,
            label=key,
        )
        ax_tp.annotate(
            "%s\n%.1f req/s, p99 %.0f ms" % (key, np.median(rates), np.median(p99s)),
            xy=(np.median(rates), np.median(p99s)),
            xytext=(10, 6) if key == "open loop" else (-10, -6),
            textcoords="offset points",
            ha="left" if key == "open loop" else "right",
            va="bottom" if key == "open loop" else "top",
            fontsize=7.5,
            color=style["color"],
        )
    # Join the two clusters, because the reader is meant to travel between them.
    # Down and to the right is the wrong direction for any real change to a
    # system, and here it comes from swapping the generator alone.
    ax_tp.annotate(
        "",
        xy=centre["closed loop"],
        xytext=centre["open loop"],
        arrowprops=dict(
            arrowstyle="-|>",
            color=GREY,
            lw=1.0,
            shrinkA=13,
            shrinkB=13,
            connectionstyle="arc3,rad=-0.22",
        ),
    )
    mid_x = (centre["open loop"][0] + centre["closed loop"][0]) / 2.0
    mid_y = math.sqrt(centre["open loop"][1] * centre["closed loop"][1])
    ax_tp.text(
        mid_x + 0.7,
        mid_y * 1.55,
        "swap the generator:\n%+d%% throughput,\np99 divided by %.0f"
        % (
            round(100.0 * (centre["closed loop"][0] / centre["open loop"][0] - 1.0)),
            centre["open loop"][1] / centre["closed loop"][1],
        ),
        fontsize=7.5,
        color=GREY,
        ha="left",
        va="center",
    )

    ax_tp.set_yscale("log")
    ax_tp.set_xlabel("achieved throughput (requests/s)")
    ax_tp.set_ylabel("p99 TTFT (ms)")
    ax_tp.set_xlim(33, 50)
    ax_tp.set_ylim(8, 400)
    ax_tp.yaxis.set_major_locator(FixedLocator([10, 20, 50, 100, 200]))
    ax_tp.yaxis.set_major_formatter(FuncFormatter(lambda v, _p: "%g" % v))
    ax_tp.yaxis.set_minor_locator(NullLocator())
    ax_tp.grid(True, alpha=0.7)
    ax_tp.set_axisbelow(True)
    titled(
        ax_tp,
        "More traffic, better tail, same worker",
        "One dot per run, large marker is the median.\n"
        "The closed-loop harness cannot be excused as a lighter load.",
    )
    return save(fig, "coordinated-omission.png")


# ---------------------------------------------------------------------------
# chart 4: what the router costs
# ---------------------------------------------------------------------------

OVERHEAD_ARMS = ["direct", "round-robin", "prefix-affinity-balanced"]


def chart_overhead():
    arms = {}
    camp = {}
    for name in OVERHEAD_ARMS:
        arm = os.path.join(RESULTS, "overhead", name)
        dirs = run_dirs(arm)
        check_valid(dirs, "overhead/%s" % name)
        arms[name] = [read_percentiles(d) for d in dirs]
        camp[name] = {
            p: campaign_metric(arm, "%s_p%s_us" % (METRIC, ("%g" % p)))
            for p in (50.0, 99.0)
        }

    fig, (ax_cdf, ax_d) = plt.subplots(
        1, 2, figsize=(10.6, 4.4), gridspec_kw={"width_ratios": [1.1, 1.0], "wspace": 0.3}
    )

    setup_tail_axis(ax_cdf)
    for name in OVERHEAD_ARMS:
        draw_arm(ax_cdf, name, arms[name])
    ax_cdf.set_xlim(3, 26)
    titled(
        ax_cdf,
        "One worker, five runs per arm",
        "50 arrivals/s at a worker with prefill switched off, so nearly\n"
        "all of this is the router and the laptop underneath it.",
    )
    ax_cdf.legend(loc="lower right", frameon=True, framealpha=0.95, edgecolor="#cccccc")

    # Right: what the router adds, with its interval. Deltas are taken against
    # the direct arm and the two half widths are combined in quadrature, which
    # is the same arithmetic the RESULTS table uses.
    rows = []
    for name in ("round-robin", "prefix-affinity-balanced"):
        for p in (99.0, 50.0):
            a = camp[name][p]
            d = camp["direct"][p]
            delta = (a["median"] - d["median"]) / 1000.0
            half = math.hypot(a["half"], d["half"]) / 1000.0
            rows.append((name, p, delta, half))

    ypos = list(range(len(rows)))[::-1]
    for y, (name, p, delta, half) in zip(ypos, rows):
        crosses_zero = (delta - half) <= 0.0 <= (delta + half)
        color = GREY if crosses_zero else STYLE[name]["color"]
        ax_d.plot(
            [delta - half, delta + half],
            [y, y],
            color=color,
            lw=2.4,
            solid_capstyle="butt",
            alpha=0.55 if crosses_zero else 0.85,
            zorder=2,
        )
        for end in (delta - half, delta + half):
            ax_d.plot([end, end], [y - 0.14, y + 0.14], color=color, lw=1.2, zorder=2)
        ax_d.scatter(
            [delta],
            [y],
            s=44,
            marker=STYLE[name]["marker"],
            color=color,
            edgecolors="white",
            linewidths=0.8,
            zorder=4,
        )
        note = "+%.2f +/-%.2f ms" % (delta, half)
        if crosses_zero:
            note += "  (contains zero)"
        # A white box behind the label, because the two reference lines run
        # straight through where the short intervals want their text.
        ax_d.annotate(
            note,
            xy=(delta, y),
            xytext=(0, 10),
            textcoords="offset points",
            ha="center",
            fontsize=7.2,
            color="#333333" if not crosses_zero else "#555555",
            bbox=dict(facecolor="white", edgecolor="none", pad=1.2, alpha=0.92),
            zorder=5,
        )

    # Zero has to be conspicuous. Three of these four intervals straddle it,
    # and that is the honest headline of the whole experiment.
    ax_d.axvline(0.0, color="#111111", lw=1.1, zorder=1)
    ax_d.axvline(1.0, color="#B22222", lw=0.9, ls=(0, (3, 2)), zorder=1)
    ax_d.text(-0.35, -0.62, "zero", fontsize=7, color="#111111", va="center", ha="right")
    ax_d.text(
        1.35,
        -0.62,
        "spec budget for p99, 1 ms",
        fontsize=7,
        color="#B22222",
        va="center",
        ha="left",
    )
    ax_d.set_yticks(ypos)
    ax_d.set_yticklabels(["%s\n%s" % (n, "p%g" % p) for n, p, _, _ in rows])
    ax_d.set_ylim(-1.05, len(rows) - 0.05)
    ax_d.set_xlim(-11.5, 12.5)
    ax_d.set_xlabel("milliseconds added by the router, versus direct")
    titled(
        ax_d,
        "Every p99 delta is smaller than its own interval",
        "Median of 5 runs, bar is mean +/- the 95% half width of the\n"
        "two arms combined in quadrature.",
    )
    ax_d.grid(True, axis="x", alpha=0.7)
    ax_d.set_axisbelow(True)
    return save(fig, "router-overhead.png")


# ---------------------------------------------------------------------------


CHART_FUNCS = (
    ("even workload tail", chart_even),
    ("skewed workload hotspot", chart_skewed),
    ("coordinated omission", chart_coordinated_omission),
    ("router overhead", chart_overhead),
)


def main():
    if not os.path.isdir(RESULTS):
        print("no results directory at %s" % RESULTS, file=sys.stderr)
        return 1
    made = []
    for title, fn in CHART_FUNCS:
        print(title)
        made.append(fn())
    print("\n%d charts in %s" % (len(made), CHARTS))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except MissingData as exc:
        # Loud on purpose. A chart drawn from half the data looks exactly like a
        # chart drawn from all of it, and that is how wrong numbers get
        # published.
        print("\nplot.py: %s" % exc, file=sys.stderr)
        sys.exit(2)
