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


def probe(song: Path) -> tuple[dict, dict]:
    """The song's tags (with lowercase keys) and its first audio stream."""
    result = subprocess.run(
        [
            "ffprobe", "-v", "error", "-select_streams", "a:0",
            "-show_entries", "format_tags:stream=codec_name,channels,sample_rate,bit_rate",
            "-of", "json", str(song),
        ],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return {}, {}
    info = json.loads(result.stdout)
    tags = {k.lower(): v for k, v in info.get("format", {}).get("tags", {}).items()}
    streams = info.get("streams") or [{}]
    return tags, streams[0]


def tags(song: Path) -> dict:
    return probe(song)[0]


def is_normalized(mp3: Path, target: float) -> bool:
    """Whether `mp3` has already been normalized to `target`."""
    tags, stream = probe(mp3)
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


def was_made_from(mp3: Path, source: Path) -> bool:
    """Whether `mp3` is the finished replacement for `source`."""
    return tags(mp3).get(FROM_TAG) == source.name


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


def normalize(song: Path, args) -> str:
    out = song.with_suffix(".mp3")
    song_tags, stream = probe(song)
    if not stream:
        raise RuntimeError("no audio track")
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
    args = parser.parse_args()

    # Temp files left by an interrupted run.
    for tmp in args.music.rglob(f".*{TEMP_SUFFIX}"):
        tmp.unlink()

    songs = []
    claimed = set()  # .mp3 names being written this run
    for song in sorted(args.music.rglob("*")):
        if not song.is_file() or song.suffix.lower() not in SONG_EXTENSIONS:
            continue
        mp3 = song.with_suffix(".mp3")
        if song == mp3:
            if not is_normalized(song, args.target):
                songs.append(song)
        elif mp3.exists() and was_made_from(mp3, song):
            # Converted before an interruption, but the original wasn't deleted yet.
            song.unlink()
            print(f"removed already converted {song}")
        elif mp3.exists() or mp3 in claimed:
            # A different song already has this .mp3 name; don't overwrite it.
            print(f"SKIPPED {song}: {mp3.name} already exists", file=sys.stderr)
        else:
            songs.append(song)
            claimed.add(mp3)

    print(f"{len(songs)} songs to normalize with {args.jobs} jobs")
    failed = 0
    with ThreadPoolExecutor(args.jobs) as pool:
        futures = {pool.submit(normalize, song, args): song for song in songs}
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
