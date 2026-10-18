#!/usr/bin/env python3
"""Normalize every song in a music folder to the same loudness.

Uses ffmpeg's two-pass loudnorm (EBU R128): the first pass measures each
song, the second applies a single linear gain so the song reaches the target
loudness without changing its dynamics.

Songs are also converted to the format the server streams (64k mono MP3, see
the ffmpeg command in src/station.rs), so every output file is a .mp3.

Originals are never modified. Normalized copies are written to a separate
folder with the same station layout and filenames (apart from the extension),
and other files (like each station's image.png) are copied across. Point
MUSIC_PATH at the output folder once you're happy with it.

Re-running skips songs that already exist in the output, so an interrupted
run can be resumed.

Usage:
    scripts/normalize_music.py music music_normalized
    scripts/normalize_music.py music music_normalized --target -14 --jobs 4
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

SONG_EXTENSIONS = {".mp3", ".ogg", ".m4a"}

# Matches what the server streams (src/station.rs). A fixed sample rate keeps
# the stream consistent when one song's MP3 data follows another's.
ENCODE = ["-c:a", "libmp3lame", "-b:a", "64k", "-ac", "1", "-ar", "44100"]

# Mix down to mono before loudnorm, so it measures and adjusts the signal
# that's actually written. Downmixing afterwards would shift the loudness.
MONO = "aformat=channel_layouts=mono"


def measure(song: Path, args) -> dict:
    """First pass: measure the song's loudness."""
    result = subprocess.run(
        [
            "ffmpeg", "-hide_banner", "-nostats", "-i", str(song),
            "-map", "0:a:0",
            "-af", f"{MONO},loudnorm=I={args.target}:TP={args.true_peak}:LRA={args.lra}:print_format=json",
            "-f", "null", "-",
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip().splitlines()[-1])
    # loudnorm prints its JSON summary as the last {...} block on stderr.
    stats = result.stderr[result.stderr.rindex("{"):]
    return json.loads(stats[: stats.index("}") + 1])


def normalize(song: Path, out: Path, args) -> str:
    stats = measure(song, args)

    if stats["input_i"] == "-inf":
        # Silent file: nothing to measure, so only convert it.
        loudnorm = MONO
    else:
        loudnorm = (
            f"{MONO},loudnorm=I={args.target}:TP={args.true_peak}:LRA={args.lra}"
            f":measured_I={stats['input_i']}:measured_TP={stats['input_tp']}"
            f":measured_LRA={stats['input_lra']}:measured_thresh={stats['input_thresh']}"
            f":offset={stats['target_offset']}:linear=true"
        )
    tmp = out.with_name(f".{out.stem}.tmp{out.suffix}")
    result = subprocess.run(
        [
            "ffmpeg", "-hide_banner", "-nostats", "-y", "-i", str(song),
            "-map", "0:a:0", "-map_metadata", "0",
            "-af", loudnorm,
            *ENCODE,
            str(tmp),
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        tmp.unlink(missing_ok=True)
        raise RuntimeError(result.stderr.strip().splitlines()[-1])

    # Write to a temp name and rename, so an interrupted run never leaves a
    # half-written file that the next run would skip.
    tmp.rename(out)
    if loudnorm == MONO:
        return f"converted (silent) {song}"
    return f"{float(stats['input_i']):6.1f} LUFS -> {args.target} LUFS  {song}"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("source", type=Path, help="music folder with one subfolder per station")
    parser.add_argument("output", type=Path, help="folder to write normalized copies to")
    parser.add_argument("--target", type=float, default=-16.0, help="integrated loudness in LUFS (default: -16)")
    parser.add_argument("--true-peak", type=float, default=-1.5, help="max true peak in dBTP (default: -1.5)")
    parser.add_argument("--lra", type=float, default=11.0, help="loudness range target (default: 11)")
    parser.add_argument("--jobs", type=int, default=os.cpu_count(), help="songs to process at once (default: CPU count)")
    args = parser.parse_args()

    source, output = args.source.resolve(), args.output.resolve()
    if output == source or output.is_relative_to(source):
        sys.exit("output must be outside the source folder")

    songs = []
    for path in sorted(source.rglob("*")):
        if not path.is_file():
            continue
        is_song = path.suffix.lower() in SONG_EXTENSIONS
        out = output / path.relative_to(source)
        if is_song:
            out = out.with_suffix(".mp3")
        out.parent.mkdir(parents=True, exist_ok=True)
        if out.exists():
            continue
        if is_song:
            songs.append((path, out))
        else:
            shutil.copy2(path, out)

    print(f"{len(songs)} songs to normalize with {args.jobs} jobs")
    failed = 0
    with ThreadPoolExecutor(args.jobs) as pool:
        futures = {pool.submit(normalize, song, out, args): song for song, out in songs}
        for done, future in enumerate(as_completed(futures), 1):
            try:
                print(f"[{done}/{len(songs)}] {future.result()}", flush=True)
            except Exception as e:
                failed += 1
                print(f"[{done}/{len(songs)}] FAILED {futures[future]}: {e}", file=sys.stderr, flush=True)

    if failed:
        sys.exit(f"{failed} songs failed")


if __name__ == "__main__":
    main()
