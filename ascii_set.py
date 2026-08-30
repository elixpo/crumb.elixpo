#!/usr/bin/env python3
"""Render Oreo as a compact ANSI outline mascot for Crumb."""

from __future__ import annotations

import argparse
import os
import sys
from dataclasses import dataclass


RESET = "\033[0m"
ART = (
    " ╭▄█▄╮   ╭▄█▄╮",
    "╭╯███╰───╯███╰╮",
    "╭╯ ▛▀▀▜═▛▀▀▜ ╰╮",
    "│  ▙▄▄▟ ▙▄▄▟  │",
    "│ ░▒▓     ▓▒░ │",
    "╰╮   ╲▁▁▁╱   ╭╯",
    " ╰───────────╯",
)


@dataclass(frozen=True)
class Color:
    red: int
    green: int
    blue: int


# A terminal-safe interpretation of the cream, charcoal, and coral favicon.
OUTLINE = Color(111, 215, 236)
EYE_PATCH = Color(91, 75, 125)
BLUSH_LIGHT = Color(255, 195, 203)
BLUSH_MID = Color(255, 126, 155)
BLUSH_DEEP = Color(235, 73, 119)
SMILE = Color(248, 190, 207)


def foreground(color: Color) -> str:
    return f"\033[38;2;{color.red};{color.green};{color.blue}m"


def character_color(character: str) -> Color | None:
    if character in "╭╮╯╰─│":
        return OUTLINE
    if character in "█▛▀▜═▙▄▟":
        return EYE_PATCH
    if character == "░":
        return BLUSH_LIGHT
    if character == "▒":
        return BLUSH_MID
    if character == "▓":
        return BLUSH_DEEP
    if character in "╲▁╱":
        return SMILE
    return None


def render(color: bool) -> str:
    """Render the five-row mascot with optional true-color ANSI styling."""
    if not color:
        return "\n".join(ART)

    rows: list[str] = []
    for line in ART:
        cells: list[str] = []
        active: Color | None = None
        for character in line:
            selected = character_color(character)
            if selected != active:
                cells.append(RESET if selected is None else foreground(selected))
                active = selected
            cells.append(character)
        cells.append(RESET)
        rows.append("".join(cells))
    return "\n".join(rows)


def use_color(mode: str) -> bool:
    if mode == "always":
        return True
    if mode == "never" or os.environ.get("NO_COLOR") is not None:
        return False
    return sys.stdout.isatty()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--color",
        choices=("auto", "always", "never"),
        default="auto",
        help="ANSI color policy (default: auto)",
    )
    args = parser.parse_args()
    print(render(use_color(args.color)))


if __name__ == "__main__":
    main()
