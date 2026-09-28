"""Tests for :mod:`hlr.io` (SPEC-0008 P-1).

Fixtures are built in ``tmp_path`` with ``zstandard``; no binary fixture is
committed and nothing is written under ``data/``.
"""

from __future__ import annotations

from pathlib import Path

import orjson
import pytest
import zstandard

from hlr.io import SegmentError, iter_envelopes, iter_frames

_LEVEL = 3


def envelope(
    seq: int,
    t_ns: int,
    *,
    src: str = "hl-ws",
    conn: str = "hl-ws-01",
    kind: str = "frame",
    raw: str = "payload",
) -> dict:
    """One envelope dict in the SPEC-0008 §5.1 shape."""
    return {
        "v": 1,
        "src": src,
        "conn": conn,
        "seq": seq,
        "t_ns": t_ns,
        "mono_ns": seq,
        "kind": kind,
        "raw": raw,
    }


def _lines(envelopes: list[dict]) -> bytes:
    return b"".join(orjson.dumps(env) + b"\n" for env in envelopes)


def _compress(raw: bytes) -> bytes:
    return zstandard.ZstdCompressor(level=_LEVEL).compress(raw)


def write_segment(path: Path, envelopes: list[dict]) -> Path:
    """Write ``envelopes`` as a finished ``.jsonl.zst`` segment."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(_compress(_lines(envelopes)))
    return path


def write_crashed_segment(path: Path, frames: list[list[dict]]) -> Path:
    """Write a ``.crashed`` segment cut mid-frame.

    ``frames`` are compressed as independent zstd frames and concatenated, then
    the last frame is cut in half, exactly like the Rust
    ``truncated_crashed_file_reads_to_last_full_line`` fixture.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    compressed = [_compress(_lines(frame)) for frame in frames]
    assert len(compressed) >= 2
    head = b"".join(compressed[:-1])
    tail = compressed[-1]
    path.write_bytes(head + tail[: len(tail) // 2])
    return path


class _Sink:
    """A write-only sink so a streaming encoder never closes our buffer."""

    def __init__(self) -> None:
        self.data = bytearray()

    def write(self, chunk: bytes) -> int:
        self.data += chunk
        return len(chunk)

    def flush(self) -> None:
        pass


def stream_compress(envelopes: list[dict]) -> bytes:
    """Compress like the Rust writer: one streaming frame, flushed as it goes.

    A streaming zstd frame has no declared content size, so its single frame
    cannot be walked by declared length. This is the shape of a real segment.
    """
    sink = _Sink()
    writer = zstandard.ZstdCompressor(level=_LEVEL).stream_writer(sink, closefd=False)
    for env in envelopes:
        writer.write(orjson.dumps(env) + b"\n")
    writer.flush()
    writer.close()
    return bytes(sink.data)


# --------------------------------------------------------------------------
# iter_envelopes
# --------------------------------------------------------------------------


def test_iter_envelopes_reads_all_fields(tmp_path: Path) -> None:
    envelopes = [
        envelope(0, 1_700_000_000_000_000_000, kind="segment_open"),
        envelope(1, 1_700_000_000_000_000_001, raw='{"channel":"bbo"}'),
        envelope(2, 1_700_000_000_000_000_002, kind="frame_bin", raw="AAECAw=="),
    ]
    path = write_segment(tmp_path / "hl-ws-01-0.jsonl.zst", envelopes)

    got = list(iter_envelopes(path))

    assert got == envelopes


def test_iter_envelopes_tolerates_truncated_crashed_stream(tmp_path: Path) -> None:
    all_envelopes = [envelope(i, 1_700_000_000_000_000_000 + i) for i in range(10)]
    path = write_crashed_segment(
        tmp_path / "hl-ws-01-0.jsonl.zst.crashed",
        [all_envelopes[:6], all_envelopes[6:]],
    )

    got = list(iter_envelopes(path))

    assert got == all_envelopes[:6]
    assert len(got) < len(all_envelopes), "fixture was not actually truncated"


def test_iter_envelopes_raises_on_truncated_finished_stream(tmp_path: Path) -> None:
    # Same damage as the crashed fixture, but a finished segment must not drop
    # data silently (SPEC-0008 G-1/G-4).
    all_envelopes = [envelope(i, 1_700_000_000_000_000_000 + i) for i in range(10)]
    path = write_crashed_segment(
        tmp_path / "hl-ws-01-0.jsonl.zst",
        [all_envelopes[:6], all_envelopes[6:]],
    )

    with pytest.raises(SegmentError, match="truncated zstd stream"):
        list(iter_envelopes(path))


def test_iter_envelopes_tolerates_truncated_crashed_single_frame(tmp_path: Path) -> None:
    # A crash usually cuts one streaming frame mid-block (SPEC-0008 §6), not a
    # concatenation of small frames. Use a large payload so the stream has many
    # blocks; decompression must stop cleanly at the last complete line.
    all_envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="x" * 300 + str(i))
        for i in range(20_000)
    ]
    compressed = _compress(_lines(all_envelopes))
    path = tmp_path / "hl-ws-01-0.jsonl.zst.crashed"
    path.write_bytes(compressed[: len(compressed) // 2])

    got = list(iter_envelopes(path))

    assert 0 < len(got) < len(all_envelopes)
    assert got == all_envelopes[: len(got)]


def test_iter_envelopes_raises_on_truncated_finished_single_frame(tmp_path: Path) -> None:
    all_envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="x" * 300 + str(i))
        for i in range(20_000)
    ]
    compressed = _compress(_lines(all_envelopes))
    path = tmp_path / "hl-ws-01-0.jsonl.zst"
    path.write_bytes(compressed[: len(compressed) // 2])

    with pytest.raises(SegmentError, match="truncated zstd stream"):
        list(iter_envelopes(path))


def test_iter_envelopes_drops_partial_final_line_in_crashed(tmp_path: Path) -> None:
    # A complete line plus a partial final line, as a crash leaves it.
    complete = [envelope(i, 1_700_000_000_000_000_000 + i) for i in range(4)]
    raw = _lines(complete) + b'{"v":1,"src":"hl-ws","conn":"hl-ws-01"'
    path = tmp_path / "hl-ws-01-0.jsonl.zst.crashed"
    path.write_bytes(_compress(raw))

    got = list(iter_envelopes(path))

    assert got == complete


def test_iter_envelopes_raises_on_partial_final_line_in_finished(tmp_path: Path) -> None:
    complete = [envelope(i, 1_700_000_000_000_000_000 + i) for i in range(4)]
    raw = _lines(complete) + b'{"v":1,"src":"hl-ws","conn":"hl-ws-01"'
    path = tmp_path / "hl-ws-01-0.jsonl.zst"
    path.write_bytes(_compress(raw))

    with pytest.raises(SegmentError, match="partial final line"):
        list(iter_envelopes(path))


def test_iter_envelopes_parses_large_multi_frame_segment(tmp_path: Path) -> None:
    # Many concatenated frames and a payload well past any single read buffer;
    # every envelope must come back intact and in order.
    envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="y" * (i * 7 % 200))
        for i in range(5_000)
    ]
    path = tmp_path / "hl-ws-01-0.jsonl.zst"
    frame_size = 500
    chunks = [
        _compress(_lines(envelopes[i : i + frame_size]))
        for i in range(0, len(envelopes), frame_size)
    ]
    path.write_bytes(b"".join(chunks))

    got = list(iter_envelopes(path))

    assert got == envelopes


def test_iter_envelopes_matches_default_chunk_at_tiny_chunk(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A 64-byte read size splits both the compressed frames and the JSON lines
    # across many reads. The result must be identical to the default chunk size,
    # proving the decompressor's frame tracking and the line carry both work
    # across chunk boundaries.
    envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="v" * (i % 120))
        for i in range(2_000)
    ]
    multi_frame = b"".join(
        _compress(_lines(envelopes[i : i + 250]))
        for i in range(0, len(envelopes), 250)
    )
    finished = tmp_path / "multi.jsonl.zst"
    finished.write_bytes(multi_frame)

    crashed_data = stream_compress(envelopes)
    crashed = tmp_path / "single.jsonl.zst.crashed"
    crashed.write_bytes(crashed_data[: len(crashed_data) // 2])

    expected_finished = list(iter_envelopes(finished))
    expected_crashed = list(iter_envelopes(crashed))
    assert expected_finished == envelopes

    monkeypatch.setattr("hlr.io._READ_CHUNK", 64)

    assert list(iter_envelopes(finished)) == expected_finished
    assert list(iter_envelopes(crashed)) == expected_crashed


def test_iter_envelopes_raises_at_tiny_chunk_on_damaged_finished(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="v" * 80)
        for i in range(1_000)
    ]
    data = stream_compress(envelopes)
    path = tmp_path / "single.jsonl.zst"
    path.write_bytes(data[: len(data) // 2])
    monkeypatch.setattr("hlr.io._READ_CHUNK", 64)

    with pytest.raises(SegmentError, match="truncated zstd stream"):
        list(iter_envelopes(path))


def test_iter_envelopes_tolerates_truncated_crashed_multi_frame(tmp_path: Path) -> None:
    # Several complete frames plus a truncated final frame: a crashed segment
    # yields the complete frames and stops cleanly.
    all_envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="z" * 100)
        for i in range(60)
    ]
    frames = [_compress(_lines(all_envelopes[i : i + 20])) for i in range(0, 60, 20)]
    truncated = b"".join(frames[:-1]) + frames[-1][: len(frames[-1]) // 2]
    path = tmp_path / "hl-ws-01-0.jsonl.zst.crashed"
    path.write_bytes(truncated)

    got = list(iter_envelopes(path))

    assert got == all_envelopes[:40]


def test_iter_envelopes_reads_writer_style_streaming_frame(tmp_path: Path) -> None:
    # The recorder emits one streaming frame with no declared content size; this
    # is the realistic on-disk shape.
    envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="w" * (i % 150))
        for i in range(2_000)
    ]
    path = tmp_path / "hl-ws-01-0.jsonl.zst"
    path.write_bytes(stream_compress(envelopes))

    got = list(iter_envelopes(path))

    assert got == envelopes


def test_iter_envelopes_raises_on_truncated_finished_streaming_frame(
    tmp_path: Path,
) -> None:
    envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="w" * 200) for i in range(1_000)
    ]
    data = stream_compress(envelopes)
    path = tmp_path / "hl-ws-01-0.jsonl.zst"
    path.write_bytes(data[: len(data) // 2])

    with pytest.raises(SegmentError, match="truncated zstd stream"):
        list(iter_envelopes(path))


def test_iter_envelopes_truncated_crashed_streaming_frame_stops_cleanly(
    tmp_path: Path,
) -> None:
    # A truncated single streaming frame can only be dropped whole (libzstd
    # yields nothing past the cut), but a crashed segment must not raise.
    envelopes = [
        envelope(i, 1_700_000_000_000_000_000 + i, raw="w" * 200) for i in range(1_000)
    ]
    data = stream_compress(envelopes)
    path = tmp_path / "hl-ws-01-0.jsonl.zst.crashed"
    path.write_bytes(data[: len(data) // 2])

    got = list(iter_envelopes(path))

    assert got == []


# --------------------------------------------------------------------------
# iter_frames
# --------------------------------------------------------------------------


def _build_tree(root: Path) -> dict[str, list[dict]]:
    """Build a two-day ``hl-ws`` tree plus out-of-range/other-source segments."""
    day1 = [envelope(n, 1_767_225_600_000_000_000 + n) for n in range(2)]
    day2 = [envelope(n + 10, 1_767_312_000_000_000_000 + n) for n in range(2)]
    day5 = [envelope(n + 20, 1_767_571_200_000_000_000 + n) for n in range(1)]
    other = [
        envelope(
            n,
            1_767_225_600_000_000_000 + n,
            src="binance-usdm",
            conn="binance-usdm-01",
        )
        for n in range(1)
    ]

    write_segment(root / "hl-ws/2026-01-01/00/hl-ws-01-100.jsonl.zst", day1)
    write_crashed_segment(
        root / "hl-ws/2026-01-02/03/hl-ws-01-200.jsonl.zst.crashed",
        [day2[:1], day2[1:]],
    )
    write_segment(root / "hl-ws/2026-01-05/00/hl-ws-01-300.jsonl.zst", day5)
    write_segment(
        root / "binance-usdm/2026-01-01/00/binance-usdm-01-400.jsonl.zst", other
    )
    # A manifest must never be yielded.
    (root / "hl-ws/2026-01-01/manifest.jsonl").write_text("{}\n")
    return {"day1": day1, "day2": day2, "day5": day5, "other": other}


def test_iter_frames_enumerates_inclusive_date_range(tmp_path: Path) -> None:
    root = tmp_path / "mainnet"
    expected = _build_tree(root)

    got = list(iter_frames(root, "hl-ws", "2026-01-01", "2026-01-02"))

    assert got == expected["day1"] + expected["day2"][:1]
    assert expected["day5"][0] not in got
    assert expected["other"][0] not in got


def test_iter_frames_includes_crashed_and_skips_manifest(tmp_path: Path) -> None:
    root = tmp_path / "mainnet"
    _build_tree(root)

    paths = [
        env["t_ns"] for env in iter_frames(root, "hl-ws", "2026-01-02", "2026-01-02")
    ]

    assert paths == [1_767_312_000_000_000_000]


def test_iter_frames_single_day_excludes_others(tmp_path: Path) -> None:
    root = tmp_path / "mainnet"
    expected = _build_tree(root)

    got = list(iter_frames(root, "hl-ws", "2026-01-01", "2026-01-01"))

    assert got == expected["day1"]


def test_iter_frames_missing_root_or_src_is_empty(tmp_path: Path) -> None:
    root = tmp_path / "mainnet"
    (root / "hl-ws/2026-01-01/00").mkdir(parents=True)

    assert list(iter_frames(root, "hl-ws", "2026-01-01", "2026-01-01")) == []
    assert list(iter_frames(root, "binance-usdm", "2026-01-01", "2026-01-01")) == []
    assert list(iter_frames(tmp_path / "nope", "hl-ws", "2026-01-01", "2026-01-01")) == []


def test_iter_frames_reversed_range_is_empty(tmp_path: Path) -> None:
    root = tmp_path / "mainnet"
    _build_tree(root)

    assert list(iter_frames(root, "hl-ws", "2026-01-05", "2026-01-01")) == []


def test_iter_frames_rejects_bad_date(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="invalid date"):
        list(iter_frames(tmp_path, "hl-ws", "2026-13-01", "2026-01-02"))


def test_iter_frames_visits_segments_in_sorted_path_order(tmp_path: Path) -> None:
    root = tmp_path / "mainnet"
    early = [envelope(0, 1_767_225_600_000_000_000)]
    late = [envelope(1, 1_767_225_700_000_000_000)]
    # Write in reverse path order; enumeration must still return path order.
    write_segment(root / "hl-ws/2026-01-01/02/hl-ws-01-zzz.jsonl.zst", late)
    write_segment(root / "hl-ws/2026-01-01/02/hl-ws-01-aaa.jsonl.zst", early)

    got = [env["seq"] for env in iter_frames(root, "hl-ws", "2026-01-01", "2026-01-01")]

    assert got == [0, 1]


def test_iter_frames_propagates_segment_error(tmp_path: Path) -> None:
    root = tmp_path / "mainnet"
    all_envelopes = [envelope(i, 1_700_000_000_000_000_000 + i) for i in range(10)]
    write_crashed_segment(
        root / "hl-ws/2026-01-01/00/hl-ws-01-100.jsonl.zst",
        [all_envelopes[:6], all_envelopes[6:]],
    )

    with pytest.raises(SegmentError, match="truncated zstd stream"):
        list(iter_frames(root, "hl-ws", "2026-01-01", "2026-01-01"))
