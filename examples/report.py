"""Sums sales.csv by region and prints a small report."""

import csv
from collections import defaultdict
from pathlib import Path


def totals_by_region(path: Path) -> dict[str, float]:
    totals: dict[str, float] = defaultdict(float)
    with path.open(newline="") as f:
        for row in csv.DictReader(f):
            totals[row["Region"]] += float(row["Total"])
    return dict(totals)


if __name__ == "__main__":
    for region, total in sorted(totals_by_region(Path("sales.csv")).items()):
        print(f"{region:<8} {total:>10.2f}")
