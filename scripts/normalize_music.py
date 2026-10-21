#!/usr/bin/env python3
"""Normalize every song in a music folder to the same loudness, in place.

Uses ffmpeg's two-pass loudnorm (EBU R128): the first pass measures each
song, the second applies a single linear gain so the song reaches the target
loudness without changing its dynamics.

Songs are also converted to the format the server streams (64k mono MP3, see
the ffmpeg command in src/station.rs), which lets the server copy them
straight through instead of re-encoding.

Each song is REPLACED by its normalized .mp3 version: an .mp3 is overwritten,
and any other format (.ogg, .m4a, .opus, or a .webm/.mp4/.mkv video, whose first
audio track is used) is deleted once its .mp3 has been written. Other files (like
each station's image.png) are left alone.

Songs shorter than --min-length seconds (default 60) are DELETED.

Large libraries (tens of thousands of songs) start straight away: the folder
scan only lists files, and every ffprobe/ffmpeg call runs in the worker pool.
Already-normalized songs aren't printed; a progress line is shown every
--progress-every songs instead.

Safe to interrupt and re-run:
  - Songs are written to a hidden .normalizing temp file and renamed into
    place, so a song is never left half-written.
  - Finished songs are tagged with the target loudness and their original
    filename. Re-runs skip them, and finish deleting an original whose .mp3
    was written just before an interruption.
  - Untagged songs already in the streamed format (written by earlier
    versions of this script) are also skipped.

Stop the server while this runs: it can fail if a song is replaced between
it listing the station's songs and opening one.

Usage:
    scripts/normalize_music.py music
    scripts/normalize_music.py music --target -14 --jobs 4
    scripts/normalize_music.py music --min-length 0   # keep short songs
"""

import argparse
import json
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

# Video files work too: only their first audio track is used.
SONG_EXTENSIONS = {".mp3", ".ogg", ".m4a", ".opus", ".webm", ".mp4", ".mkv"}

# Matches what the server streams (src/station.rs). A fixed sample rate keeps
# the stream consistent when one song's MP3 data follows another's.
ENCODE = ["-c:a", "libmp3lame", "-b:a", "64k", "-ac", "1", "-ar", "44100"]

# Mix down to mono before loudnorm, so it measures and adjusts the signal
# that's actually written. Downmixing afterwards would shift the loudness.
MONO = "aformat=channel_layouts=mono"

# ID3 tags marking a finished song: the target it was normalized to, and the
# file it was made from.
LUFS_TAG = "normalized_lufs"
FROM_TAG = "normalized_from"

TEMP_SUFFIX = ".normalizing"


def probe(song: Path) -> tuple[dict, dict, float | None]:
    """The song's tags (with lowercase keys), its first audio stream, and its
    length in seconds (None if ffprobe can't tell)."""
    result = subprocess.run(
        [
            "ffprobe", "-v", "error", "-select_streams", "a:0",
            "-show_entries", "format=duration:format_tags:stream=codec_name,channels,sample_rate,bit_rate",
            "-of", "json", str(song),
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return {}, {}, None
    info = json.loads(result.stdout)
    fmt = info.get("format", {})
    tags = {k.lower(): v for k, v in fmt.get("tags", {}).items()}
    streams = info.get("streams") or [{}]
    try:
        duration = float(fmt["duration"])
    except (KeyError, ValueError):
        duration = None
    return tags, streams[0], duration


def is_normalized(tags: dict, stream: dict, target: float) -> bool:
    """Whether an .mp3 with these tags and audio stream has already been
    normalized to `target`."""
    if LUFS_TAG in tags:
        try:
            return float(tags[LUFS_TAG]) == target
        except ValueError:
            return False
    # Earlier versions of this script didn't tag their output. None of the
    # original songs are 64k mono MP3s, so a file in the streamed format
    # without the tag was already normalized by one of those versions.
    return (
        stream.get("codec_name") == "mp3"
        and stream.get("channels") == 1
        and stream.get("sample_rate") == "44100"
        and stream.get("bit_rate") == "64000"
    )


def measure(song: Path, args) -> dict:
    """First pass: measure the song's loudness."""
    result = subprocess.run(
        [
            "ffmpeg", "-hide_banner", "-nostats", "-nostdin", "-i", str(song),
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


class Skipped(Exception):
    """A song left alone on purpose; reported, but not counted as a failure."""


def process(song: Path, mp3_existed: bool, args) -> str | None:
    """Handle one song. Returns what was done, or None if nothing was needed."""
    out = song.with_suffix(".mp3")
    if song != out and mp3_existed:
        # Its .mp3 was there when the folder was scanned: either this song
        # was converted before an interruption but not deleted yet, or a
        # different song already has that .mp3 name.
        if probe(out)[0].get(FROM_TAG) == song.name:
            song.unlink()
            return f"removed already converted {song}"
        raise Skipped(f"{out.name} already exists")

    song_tags, stream, duration = probe(song)
    if not stream:
        raise RuntimeError("no audio track")
    if duration is not None and duration < args.min_length:
        song.unlink()
        return f"deleted short song ({duration:.0f}s) {song}"
    if song == out and is_normalized(song_tags, stream, args.target):
        return None
    # Re-normalizing a converted .mp3 keeps the name of the file it came from.
    made_from = song_tags.get(FROM_TAG, song.name) if song == out else song.name
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
    # Not named .mp3, so the server never picks up a half-written file.
    tmp = song.with_name(f".{song.stem}{TEMP_SUFFIX}")
    result = subprocess.run(
        [
            "ffmpeg", "-hide_banner", "-nostats", "-nostdin", "-y", "-i", str(song),
            "-map", "0:a:0", "-map_metadata", "0",
            "-af", loudnorm,
            *ENCODE,
            "-metadata", f"{LUFS_TAG}={args.target}",
            "-metadata", f"{FROM_TAG}={made_from}",
            "-f", "mp3", str(tmp),
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        tmp.unlink(missing_ok=True)
        raise RuntimeError(result.stderr.strip().splitlines()[-1])

    # Replace the original: rename over an .mp3, or write the .mp3 then delete
    # the original (.ogg, .webm, ...). If interrupted in between, the next run sees the tagged
    # .mp3 and finishes deleting the original.
    tmp.replace(out)
    if song != out:
        song.unlink()

    if loudnorm == MONO:
        return f"converted (silent) {song}"
    return f"{float(stats['input_i']):6.1f} LUFS -> {args.target} LUFS  {song}"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("music", type=Path, help="music folder with one subfolder per station")
    parser.add_argument("--target", type=float, default=-16.0, help="integrated loudness in LUFS (default: -16)")
    parser.add_argument("--true-peak", type=float, default=-1.5, help="max true peak in dBTP (default: -1.5)")
    parser.add_argument("--lra", type=float, default=11.0, help="loudness range target (default: 11)")
    parser.add_argument("--jobs", type=int, default=os.cpu_count(), help="songs to process at once (default: CPU count)")
    parser.add_argument("--min-length", type=float, default=60.0, help="delete songs shorter than this many seconds; 0 keeps all (default: 60)")
    parser.add_argument("--progress-every", type=int, default=500, help="print a progress line every N songs checked (default: 500)")
    args = parser.parse_args()

    # Only list files here (no ffprobe), so startup stays quick on huge
    # libraries. Everything that needs to open a song happens in the pool.
    files = []
    for root, _, names in os.walk(args.music):
        for name in names:
            path = Path(root, name)
            if name.endswith(TEMP_SUFFIX):
                path.unlink()  # left by an interrupted run
            elif path.suffix.lower() in SONG_EXTENSIONS:
                files.append(path)
    files.sort()
    existing = set(files)

    songs = []  # (song, whether its .mp3 already existed)
    claimed = set()  # .mp3 names being written this run
    for song in files:
        mp3 = song.with_suffix(".mp3")
        if song == mp3 or mp3 in existing:
            songs.append((song, mp3 in existing))
        elif mp3 in claimed:
            # Two originals (say a.webm and a.ogg) would both become a.mp3.
            print(f"SKIPPED {song}: {mp3.name} is already being written", file=sys.stderr)
        else:
            songs.append((song, False))
            claimed.add(mp3)

    total = len(songs)
    print(f"{total} songs to check with {args.jobs} jobs", flush=True)
    failed = 0
    changed = 0
    with ThreadPoolExecutor(args.jobs) as pool:
        futures = {pool.submit(process, song, mp3_existed, args): song for song, mp3_existed in songs}
        for done, future in enumerate(as_completed(futures), 1):
            try:
                result = future.result()
                if result:
                    changed += 1
                    print(f"[{done}/{total}] {result}", flush=True)
            except Skipped as e:
                print(f"[{done}/{total}] SKIPPED {futures[future]}: {e}", file=sys.stderr, flush=True)
            except Exception as e:
                failed += 1
                print(f"[{done}/{total}] FAILED {futures[future]}: {e}", file=sys.stderr, flush=True)
            if done % args.progress_every == 0 or done == total:
                print(f"[{done}/{total}] checked, {changed} changed, {failed} failed", flush=True)

    if failed:
        sys.exit(f"{failed} songs failed")


if __name__ == "__main__":
    main()
