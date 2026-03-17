#!/usr/bin/env python3
"""Compute CVSS v3.1 base score from a vector string.

Usage:
  python3 scripts/cvss_calc.py "CVSS:3.1/AV:L/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"
  python3 scripts/cvss_calc.py --vector "CVSS:3.1/..."
"""

from __future__ import annotations

import argparse
import math
import sys


AV = {"N": 0.85, "A": 0.62, "L": 0.55, "P": 0.20}
AC = {"L": 0.77, "H": 0.44}
UI = {"N": 0.85, "R": 0.62}
CIA = {"N": 0.00, "L": 0.22, "H": 0.56}
PR_SCOPE_U = {"N": 0.85, "L": 0.62, "H": 0.27}
PR_SCOPE_C = {"N": 0.85, "L": 0.68, "H": 0.50}


def roundup_1dp(x: float) -> float:
    return math.ceil(x * 10.0) / 10.0


def parse_vector(vector: str) -> dict[str, str]:
    if not vector.startswith("CVSS:3.1/"):
        raise ValueError("Vector must start with CVSS:3.1/")

    metrics: dict[str, str] = {}
    parts = vector.split("/")[1:]
    for part in parts:
        if ":" not in part:
            raise ValueError(f"Invalid metric segment: {part}")
        key, value = part.split(":", 1)
        metrics[key] = value

    required = ["AV", "AC", "PR", "UI", "S", "C", "I", "A"]
    missing = [m for m in required if m not in metrics]
    if missing:
        raise ValueError(f"Missing metrics: {', '.join(missing)}")

    return metrics


def compute_cvss31_base(vector: str) -> tuple[float, str, float, float]:
    m = parse_vector(vector)

    iss = 1.0 - ((1.0 - CIA[m["C"]]) * (1.0 - CIA[m["I"]]) * (1.0 - CIA[m["A"]]))

    if m["S"] == "U":
        impact = 6.42 * iss
        pr = PR_SCOPE_U[m["PR"]]
    elif m["S"] == "C":
        impact = 7.52 * (iss - 0.029) - 3.25 * ((iss - 0.02) ** 15)
        pr = PR_SCOPE_C[m["PR"]]
    else:
        raise ValueError("S must be U or C")

    exploitability = 8.22 * AV[m["AV"]] * AC[m["AC"]] * pr * UI[m["UI"]]

    if impact <= 0:
        base = 0.0
    elif m["S"] == "U":
        base = roundup_1dp(min(impact + exploitability, 10.0))
    else:
        base = roundup_1dp(min(1.08 * (impact + exploitability), 10.0))

    if base == 0.0:
        severity = "NONE"
    elif base <= 3.9:
        severity = "LOW"
    elif base <= 6.9:
        severity = "MEDIUM"
    elif base <= 8.9:
        severity = "HIGH"
    else:
        severity = "CRITICAL"

    return base, severity, exploitability, impact


def main() -> int:
    parser = argparse.ArgumentParser(description="Compute CVSS v3.1 base score")
    parser.add_argument("vector", nargs="?", help="CVSS vector string")
    parser.add_argument("--vector", dest="vector_flag", help="CVSS vector string")
    args = parser.parse_args()

    vector = args.vector_flag or args.vector
    if not vector:
        print("error: provide a CVSS:3.1 vector", file=sys.stderr)
        return 2

    try:
        base, severity, exploitability, impact = compute_cvss31_base(vector)
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    print(f"vector={vector}")
    print(f"base_score={base:.1f}")
    print(f"severity={severity}")
    print(f"exploitability={exploitability:.4f}")
    print(f"impact={impact:.4f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
