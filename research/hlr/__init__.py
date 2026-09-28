"""Research toolkit for the Hyperliquid market-data recorder (SPEC-0008).

This package reads the raw segment files the Rust recorder writes and turns them
into research tables and studies. It is research code: never imported by, or
deployed with, the production bot.
"""

from hlr.io import iter_envelopes, iter_frames

__all__ = ["iter_envelopes", "iter_frames"]
