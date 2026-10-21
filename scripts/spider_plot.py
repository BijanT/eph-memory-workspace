#!/usr/bin/env python3
"""Spider plot comparing ephemeral memory to the state of the art.

Usage: spider_plot.py <output.png>
"""
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

ATTRIBUTES = [
    "Application\nTransparency",
    "Reclamation\nSpeed",
    "Overcommitment\nAggressiveness",
    "Generality",
]

# name -> (color, values in the same order as ATTRIBUTES)
SYSTEMS = {
    "Memory Ballooning": ("red", [5, 2, 2, 5]),
    "Memory Hot (Un)plug": ("blue", [4.5, 1.5, 1.5, 5]),
    "Ephemeral Memory": ("green", [1, 5, 5, 1]),
}

MAX_VALUE = 5


def main():
    if len(sys.argv) != 2:
        sys.exit(f"Usage: {sys.argv[0]} <output.png>")
    out_path = sys.argv[1]

    angles = np.linspace(0, 2 * np.pi, len(ATTRIBUTES), endpoint=False)
    angles = np.concatenate([angles, angles[:1]])  # close the polygon

    fig, ax = plt.subplots(figsize=(8, 8), subplot_kw={"polar": True})
    ax.set_theta_offset(np.pi / 2)  # first attribute at the top
    ax.set_theta_direction(-1)

    for name, (color, values) in SYSTEMS.items():
        vals = values + values[:1]
        ax.plot(angles, vals, color=color, linewidth=2, label=name)
        ax.fill(angles, vals, color=color, alpha=0.12)

    ax.set_xticks(angles[:-1])
    labels = ax.set_xticklabels(ATTRIBUTES, fontsize=18)
    ax.tick_params(axis="x", pad=18)
    # Labels on the left/right sides are centered on the axis by default, which
    # makes them overlap the plot; anchor them outward instead.
    for label, angle in zip(labels, angles[:-1]):
        side = np.sin(angle)
        if side > 0.5:
            label.set_horizontalalignment("left")
        elif side < -0.5:
            label.set_horizontalalignment("right")
    ax.set_ylim(0, MAX_VALUE)
    ax.set_yticks(range(1, MAX_VALUE + 1))
    ax.set_yticklabels([])  # values are relative, so omit the numeric labels
    ax.grid(color="gray", alpha=0.4)

    ax.legend(loc="upper center", bbox_to_anchor=(0.5, -0.08), ncol=3,
              frameon=False, fontsize=16)

    fig.savefig(out_path, dpi=200, bbox_inches="tight")


if __name__ == "__main__":
    main()
