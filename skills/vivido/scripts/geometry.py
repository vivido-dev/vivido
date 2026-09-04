#!/usr/bin/env python3
"""Convert between terminal cells and physical pixels for a Vivido window.

Reads the JSON printed by `vivido msg capture`, `vivido msg screenshot --json`, or
`vivido msg inspect`, and answers the questions that go wrong when done by hand:

    vivido msg capture --window-id 42 --stable > cap.json

    # where do I click for the cell at column 12, row 4?
    ./geometry.py cell-to-pixel --from cap.json --column 12 --row 4

    # what cell did the user mean by this pixel in the screenshot?
    ./geometry.py pixel-to-cell --from cap.json --x 320 --y 180

    # crop the screenshot down to a status bar before showing it to a vision model
    ./geometry.py crop-box --from cap.json --rows 22:23 --out status.png

The padding is read from the response and never derived. With `dynamic_padding` off -- the
default -- the sub-cell remainder collects at the right and bottom rather than being split, so
`(width - columns * cell_width) / 2` over-estimates the origin by half the remainder.

Everything prints one JSON object. Errors print `{"error": "..."}` and exit 2.
"""

from __future__ import annotations

import argparse
import json
import math
import sys


class GeometryError(Exception):
    """A message that is useful to the caller rather than a traceback."""


class Geometry:
    """Normalized cell/pixel geometry from any of the three response shapes."""

    def __init__(self, document: dict) -> None:
        window = document.get("window")
        source = window if isinstance(window, dict) else document

        pixels = source.get("pixels", source)
        self.width = _positive(pixels, "width", "pixel width")
        self.height = _positive(pixels, "height", "pixel height")

        cell = _object(document, "cell")
        self.cell_width = _positive(cell, "width", "cell width")
        self.cell_height = _positive(cell, "height", "cell height")

        # Not derivable from the values above; see the module docstring.
        padding = _object(source, "padding")
        self.padding_x = float(padding.get("x", 0.0))
        self.padding_y = float(padding.get("y", 0.0))

        grid = source.get("grid")
        if isinstance(grid, dict) and "columns" in grid and "rows" in grid:
            self.columns = int(grid["columns"])
            self.rows = int(grid["rows"])
            self.grid_is_derived = False
        else:
            # A screenshot response carries no grid. Bound it from the capture instead, and say so,
            # rather than letting a caller believe Vivido reported these.
            self.columns = max(0, int((self.width - self.padding_x) // self.cell_width))
            self.rows = max(0, int((self.height - self.padding_y) // self.cell_height))
            self.grid_is_derived = True

        self.scale_factor = document.get("scale_factor")
        self.frame_sequence = document.get("frame_sequence")
        self.window_id = document.get("window_id", source.get("window_id"))
        self.path = document.get("path")

    def cell_origin(self, column: int, row: int) -> tuple[float, float]:
        """Top-left physical pixel of a zero-based cell."""
        return (
            self.padding_x + column * self.cell_width,
            self.padding_y + row * self.cell_height,
        )

    def cell_center(self, column: int, row: int) -> tuple[float, float]:
        x, y = self.cell_origin(column, row)
        return (x + self.cell_width / 2.0, y + self.cell_height / 2.0)

    def cell_at(self, x: float, y: float) -> tuple[int, int]:
        column = math.floor((x - self.padding_x) / self.cell_width)
        row = math.floor((y - self.padding_y) / self.cell_height)
        return (column, row)

    def contains_cell(self, column: int, row: int) -> bool:
        return 0 <= column < self.columns and 0 <= row < self.rows

    def describe(self) -> dict:
        return {
            "window_id": self.window_id,
            "frame_sequence": self.frame_sequence,
            "pixels": {"width": self.width, "height": self.height},
            "cell": {"width": self.cell_width, "height": self.cell_height},
            "padding": {"x": self.padding_x, "y": self.padding_y},
            "grid": {"columns": self.columns, "rows": self.rows},
            "grid_is_derived": self.grid_is_derived,
            "scale_factor": self.scale_factor,
            "path": self.path,
        }


def _object(document: dict, key: str) -> dict:
    value = document.get(key)
    if not isinstance(value, dict):
        raise GeometryError(
            f"input has no {key!r} object; expected the JSON from "
            "`vivido msg capture`, `screenshot --json`, or `inspect`"
        )
    return value


def _positive(document: dict, key: str, label: str) -> float:
    value = document.get(key)
    if not isinstance(value, (int, float)) or value <= 0:
        raise GeometryError(f"{label} must be a positive number, got {value!r}")
    return float(value)


def _load(path: str) -> Geometry:
    if path == "-":
        text = sys.stdin.read()
    else:
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
    if not text.strip():
        raise GeometryError("no input; pipe the JSON in, or pass --from PATH")
    try:
        document = json.loads(text)
    except json.JSONDecodeError as error:
        raise GeometryError(f"input is not JSON: {error}") from error
    if not isinstance(document, dict):
        raise GeometryError("input must be one JSON object")
    return Geometry(document)


def _parse_range(text: str, label: str) -> tuple[int, int]:
    """`N` is one index; `A:B` is an inclusive range."""
    parts = text.split(":")
    try:
        values = [int(part) for part in parts]
    except ValueError as error:
        raise GeometryError(f"{label} must be N or A:B, got {text!r}") from error
    if len(values) == 1:
        first = last = values[0]
    elif len(values) == 2:
        first, last = values
    else:
        raise GeometryError(f"{label} must be N or A:B, got {text!r}")
    if last < first:
        raise GeometryError(f"{label} range is inverted: {text!r}")
    return first, last


def command_info(geometry: Geometry, _arguments: argparse.Namespace) -> dict:
    return geometry.describe()


def command_cell_to_pixel(geometry: Geometry, arguments: argparse.Namespace) -> dict:
    column, row = arguments.column, arguments.row
    if column < 0 or row < 0:
        raise GeometryError("cell coordinates are zero-based and cannot be negative")
    origin_x, origin_y = geometry.cell_origin(column, row)
    center_x, center_y = geometry.cell_center(column, row)
    return {
        "cell": {"column": column, "row": row},
        "top_left": {"x": round(origin_x), "y": round(origin_y)},
        "center": {"x": round(center_x), "y": round(center_y)},
        "exact": {
            "top_left": {"x": origin_x, "y": origin_y},
            "center": {"x": center_x, "y": center_y},
        },
        "within_grid": geometry.contains_cell(column, row),
        "grid": {"columns": geometry.columns, "rows": geometry.rows},
        "grid_is_derived": geometry.grid_is_derived,
    }


def command_pixel_to_cell(geometry: Geometry, arguments: argparse.Namespace) -> dict:
    column, row = geometry.cell_at(arguments.x, arguments.y)
    result = {
        "pixel": {"x": arguments.x, "y": arguments.y},
        "cell": {"column": column, "row": row},
        "within_grid": geometry.contains_cell(column, row),
        "grid": {"columns": geometry.columns, "rows": geometry.rows},
        "grid_is_derived": geometry.grid_is_derived,
    }
    if arguments.x < geometry.padding_x or arguments.y < geometry.padding_y:
        result["note"] = "pixel is inside the padding, before the grid origin"
    return result


def command_crop_box(geometry: Geometry, arguments: argparse.Namespace) -> dict:
    if arguments.columns is None and arguments.rows is None:
        raise GeometryError("crop-box needs --columns, --rows, or both")
    first_column, last_column = (
        _parse_range(arguments.columns, "--columns")
        if arguments.columns
        else (0, geometry.columns - 1)
    )
    first_row, last_row = (
        _parse_range(arguments.rows, "--rows") if arguments.rows else (0, geometry.rows - 1)
    )

    left, top = geometry.cell_origin(first_column, first_row)
    right, bottom = geometry.cell_origin(last_column + 1, last_row + 1)
    left, top = max(0.0, left), max(0.0, top)
    right, bottom = min(float(geometry.width), right), min(float(geometry.height), bottom)
    if right <= left or bottom <= top:
        raise GeometryError("the requested cells fall outside the captured image")

    box = {
        "left": round(left),
        "top": round(top),
        "right": round(right),
        "bottom": round(bottom),
        "width": round(right - left),
        "height": round(bottom - top),
    }
    result = {
        "cells": {
            "columns": [first_column, last_column],
            "rows": [first_row, last_row],
        },
        "box": box,
        "clamped": (
            first_column < 0
            or first_row < 0
            or last_column >= geometry.columns
            or last_row >= geometry.rows
        ),
        "grid_is_derived": geometry.grid_is_derived,
        "source": geometry.path,
    }
    if arguments.out:
        result["out"] = _write_crop(geometry, box, arguments.out)
    return result


def _write_crop(geometry: Geometry, box: dict, destination: str) -> str:
    if not geometry.path:
        raise GeometryError("--out needs a capture whose JSON carries the PNG `path`")
    try:
        from PIL import Image  # noqa: PLC0415 -- optional, only for --out
    except ImportError as error:
        raise GeometryError(
            "--out needs Pillow; without it, use the printed `box` with any image tool"
        ) from error
    with Image.open(geometry.path) as image:
        image.crop((box["left"], box["top"], box["right"], box["bottom"])).save(destination)
    return destination


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--from",
        dest="source",
        default="-",
        metavar="PATH",
        help="JSON from capture/screenshot/inspect; '-' (the default) reads stdin",
    )
    commands = parser.add_subparsers(dest="command", required=True)

    commands.add_parser("info", help="print the normalized geometry")

    to_pixel = commands.add_parser("cell-to-pixel", help="zero-based cell to physical pixels")
    to_pixel.add_argument("--column", type=int, required=True)
    to_pixel.add_argument("--row", type=int, required=True)

    to_cell = commands.add_parser("pixel-to-cell", help="physical pixel to zero-based cell")
    to_cell.add_argument("--x", type=float, required=True)
    to_cell.add_argument("--y", type=float, required=True)

    crop = commands.add_parser("crop-box", help="pixel box for an inclusive cell rectangle")
    crop.add_argument("--columns", metavar="N|A:B", help="default: the full width")
    crop.add_argument("--rows", metavar="N|A:B", help="default: the full height")
    crop.add_argument("--out", metavar="PATH", help="also write the crop (needs Pillow)")

    arguments = parser.parse_args(argv)
    handlers = {
        "info": command_info,
        "cell-to-pixel": command_cell_to_pixel,
        "pixel-to-cell": command_pixel_to_cell,
        "crop-box": command_crop_box,
    }
    try:
        geometry = _load(arguments.source)
        result = handlers[arguments.command](geometry, arguments)
    except GeometryError as error:
        json.dump({"error": str(error)}, sys.stdout)
        sys.stdout.write("\n")
        return 2
    except OSError as error:
        json.dump({"error": str(error)}, sys.stdout)
        sys.stdout.write("\n")
        return 2
    json.dump(result, sys.stdout)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
