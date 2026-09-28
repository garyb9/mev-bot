"""Read the recorder's on-disk segment files (SPEC-0008 §5 and §6).

The Rust crate `mev-recorder` is the ground truth for this format:

* A segment is a zstd-compressed JSON Lines file. Each line is one *envelope*
  object (`v`, `src`, `conn`, `seq`, `t_ns`, `mono_ns`, `kind`, `raw`/`meta`).
* Finished segments are named `*.jsonl.zst`; a segment the writer did not finish
  before crashing is recovered as `*.jsonl.zst.crashed` and its zstd stream is
  typically cut off mid-frame.
* Segments live under `{root}/{src}/{YYYY-MM-DD}/{HH}/`, where `root` is the
  recorder's network directory (for example `data/rec/mainnet`).

This module mirrors `mev_recorder::reader::{SegmentReader, segments_for}`: only a
``.crashed`` segment tolerates a truncated zstd tail or a partial final line (it
stops cleanly at the last complete line). A damaged *finished* segment raises
:class:`SegmentError` instead, because silently dropping data from a finished
segment would hide a recorder defect (SPEC-0008 G-1, G-4).
"""

from __future__ import annotations

import datetime as _dt
import os
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import orjson
import zstandard

__all__ = ["SegmentError", "iter_envelopes", "iter_frames"]

#: A decoded envelope: the JSON object on one segment line.
Envelope = dict[str, Any]

#: Input read size when streaming a segment; monkeypatched small in tests.
_READ_CHUNK = 1 << 20


class SegmentError(Exception):
    """A finished segment is damaged (a truncated zstd stream or a partial line).

    Raised with the offending file's path in the message. A ``.crashed`` segment
    never raises this: its truncated tail is expected and iteration stops cleanly
    at the last complete line.
    """


def iter_envelopes(path: str | os.PathLike[str]) -> Iterator[Envelope]:
    """Yield the parsed envelopes of one segment file, in file order.

    ``path`` may be a finished segment (``*.jsonl.zst``) or a crash-recovered
    one (``*.jsonl.zst.crashed``). The file is read in bounded chunks, fed to a
    streaming zstd decompressor, and split into lines incrementally, so a
    segment larger than memory is never held whole (segments rotate at 1 GiB
    *uncompressed*).

    For a ``.crashed`` segment a truncated zstd tail or a partial final line
    does **not** raise: iteration stops cleanly at the last complete,
    newline-terminated line. For a finished segment the same damage raises
    :class:`SegmentError`, so research never silently drops data from a segment
    the writer reported as complete.
    """
    path = Path(path)
    crashed = _is_crashed(path)

    state = _StreamState()
    with open(path, "rb") as handle:
        # carry: decompressed bytes of a line not yet terminated by a newline.
        carry = b""
        while True:
            chunk = handle.read(_READ_CHUNK)
            if not chunk:
                break
            for block in _feed(state, chunk):
                lines, carry = _split_lines(carry + block)
                for line in lines:
                    parsed = _decode_line(line)
                    if parsed is not None:
                        yield parsed
            if state.failed:
                # A corrupt/truncated frame: no more input can help.
                break

    # End of input. A crashed segment drops any incomplete tail without
    # raising; a finished segment must not silently lose data.
    if crashed:
        return
    if state.failed:
        raise SegmentError(f"corrupt or truncated zstd stream in finished segment {path}")
    if not state.eof:
        raise SegmentError(f"truncated zstd stream in finished segment {path}")
    if carry.strip():
        raise SegmentError(f"partial final line in finished segment {path}")


class _StreamState:
    """Mutable zstd streaming state for one segment file.

    ``obj`` is the decompressobj for the frame currently open; when it ends
    (``obj.eof``) the next frame is started. A frame that is not yet complete
    buffers its input inside ``obj``; ``failed`` records a zstd error and
    ``eof`` whether the stream ended on a frame boundary.
    """

    def __init__(self) -> None:
        self.obj = zstandard.ZstdDecompressor().decompressobj()
        self.failed = False
        # True until a frame is left open; an empty file ends clean.
        self.eof = True


def _feed(state: _StreamState, chunk: bytes) -> list[bytes]:
    """Feed one input chunk and return the decompressed blocks it produced.

    Advances ``state`` across frame boundaries and records a zstd error instead
    of raising. Returning blocks (rather than one buffer) keeps peak memory to
    one chunk plus one block. A frame that is not yet complete buffers its input
    inside the decompressobj, so only bytes after a frame boundary
    (``unused_data``) are processed here.
    """
    blocks: list[bytes] = []
    data = chunk
    while data:
        try:
            out = state.obj.decompress(data)
        except zstandard.ZstdError:
            state.failed = True
            return blocks
        if out:
            blocks.append(out)
        if not state.obj.eof:
            # Frame still open; zstd keeps its input.
            state.eof = False
            return blocks
        # A frame ended. Any bytes past its end begin the next frame.
        leftover = state.obj.unused_data
        if not leftover:
            state.eof = True
            return blocks
        if leftover == data:
            # No forward progress: a frame that consumed nothing. Stop rather
            # than loop forever.
            state.failed = True
            return blocks
        state.obj = zstandard.ZstdDecompressor().decompressobj()
        state.eof = False
        data = leftover
    return blocks


def _split_lines(buffer: bytes) -> tuple[list[bytes], bytes]:
    """Split ``buffer`` into complete lines; return them and the trailing carry.

    Linear in ``buffer``: ``split`` walks it once and the last element is the
    un-terminated remainder. The carry is empty when ``buffer`` ends in ``\\n``.
    """
    parts = buffer.split(b"\n")
    carry = parts.pop()
    return parts, carry


def _decode_line(line: bytes) -> Envelope | None:
    """Parse one non-empty line, skipping blank lines like the Rust reader."""
    line = line.rstrip(b"\r")
    if not line.strip():
        return None
    return orjson.loads(line)


def iter_frames(
    root: str | os.PathLike[str],
    src: str,
    date_from: str,
    date_to: str,
) -> Iterator[Envelope]:
    """Yield every envelope for source ``src`` under ``root`` in a date range.

    ``root`` is the recorder's network directory from the SPEC-0008 §6 layout,
    i.e. ``data/rec/{network}``; it holds one directory per source. Segments are
    ``root/{src}/{YYYY-MM-DD}/{HH}/{conn}-{start_t_ns}.jsonl.zst`` plus the
    ``.jsonl.zst.crashed`` variant.

    The UTC date range ``[date_from, date_to]`` is inclusive and the segments
    are visited in sorted-path order, the same enumeration and order as the Rust
    ``segments_for``. A missing ``root`` or ``src`` directory yields nothing.
    ``date_from`` after ``date_to`` also yields nothing. A malformed date raises
    :class:`ValueError`. A damaged finished segment propagates
    :class:`SegmentError`.
    """
    for path in _segments_for(root, src, date_from, date_to):
        yield from iter_envelopes(path)


def _segments_for(
    root: str | os.PathLike[str],
    src: str,
    date_from: str,
    date_to: str,
) -> list[Path]:
    """Enumerate segment files for ``src``, sorted by path (Rust order)."""
    from_day = _parse_date(date_from)
    to_day = _parse_date(date_to)
    if from_day > to_day:
        return []

    src_dir = Path(root) / src
    if not src_dir.is_dir():
        return []

    files: list[Path] = []
    for date_dir in src_dir.iterdir():
        if not date_dir.is_dir():
            continue
        day = _parse_date_or_none(date_dir.name)
        if day is None or day < from_day or day > to_day:
            continue
        files.extend(_collect_segments(date_dir))
    files.sort(key=str)
    return files


def _collect_segments(directory: Path) -> Iterator[Path]:
    """Yield the segment files under ``directory`` (recursively)."""
    for path in directory.rglob("*"):
        if path.is_file() and _is_segment(path):
            yield path


def _is_segment(path: Path) -> bool:
    """Whether ``path`` is a finished or crash-recovered segment file."""
    return path.name.endswith((".jsonl.zst", ".jsonl.zst.crashed"))


def _is_crashed(path: Path) -> bool:
    """Whether ``path`` is a crash-recovered segment (its tail may be cut)."""
    return path.name.endswith(".jsonl.zst.crashed")


def _parse_date(value: str) -> _dt.date:
    """Parse a ``YYYY-MM-DD`` UTC date, raising :class:`ValueError` if invalid."""
    try:
        return _dt.date.fromisoformat(value)
    except ValueError as err:
        raise ValueError(f"invalid date `{value}`") from err


def _parse_date_or_none(value: str) -> _dt.date | None:
    """Parse a date directory name, or return ``None`` if it is not a date."""
    try:
        return _dt.date.fromisoformat(value)
    except ValueError:
        return None
