"""Losslessly combine legacy mono WAV pairs into microphone/system channels.

Existing output files are never replaced. Source files remain untouched, and
every output sample is checked against its source before the final rename.
Uses only the Python standard library.
"""

import argparse
import json
from pathlib import Path
import wave


def snapshot(path):
    stat = path.stat()
    return stat.st_size, stat.st_mtime_ns


def merge_pair(mic_path, system_path):
    base = mic_path.name.removesuffix(".mic.wav")
    output = mic_path.with_name(base + ".wav")
    partial = output.with_name(output.name + ".partial")
    if output.exists() or partial.exists():
        raise FileExistsError(f"Output already exists: {output} or {partial}")

    sources = [mic_path, system_path]
    before = [snapshot(path) for path in sources]
    with wave.open(str(mic_path), "rb") as mic, wave.open(str(system_path), "rb") as system:
        readers = [mic, system]
        for reader in readers:
            if (reader.getnchannels(), reader.getsampwidth(), reader.getcomptype()) != (1, 2, "NONE"):
                raise ValueError("Each input must be a mono 16-bit PCM WAV")
        if mic.getframerate() != system.getframerate():
            raise ValueError("Sample rates differ; refusing to resample")
        rate = mic.getframerate()
        lengths = [reader.getnframes() for reader in readers]
        frames = max(lengths)
        if frames * 4 + 36 > 0xFFFFFFFF:
            raise ValueError("Recording exceeds the standard RIFF WAV size limit")

        with partial.open("xb") as file, wave.open(file, "wb") as writer:
            writer.setnchannels(2)
            writer.setsampwidth(2)
            writer.setframerate(rate)
            for offset in range(0, frames, 65536):
                count = min(65536, frames - offset)
                channels = []
                for reader, length in zip(readers, lengths):
                    wanted = min(count, max(0, length - offset))
                    samples = reader.readframes(wanted)
                    if len(samples) != wanted * 2:
                        raise ValueError("Input WAV is truncated")
                    channels.append(samples + bytes((count - wanted) * 2))
                interleaved = bytearray(count * 4)
                view = memoryview(interleaved).cast("h")
                view[0::2] = memoryview(channels[0]).cast("h")
                view[1::2] = memoryview(channels[1]).cast("h")
                writer.writeframesraw(interleaved)

        for reader in readers:
            reader.rewind()
        with wave.open(str(partial), "rb") as merged:
            if (merged.getnchannels(), merged.getsampwidth(), merged.getframerate(), merged.getnframes()) != (2, 2, rate, frames):
                raise ValueError("Output WAV header verification failed")
            for offset in range(0, frames, 65536):
                count = min(65536, frames - offset)
                data = merged.readframes(count)
                if len(data) != count * 4:
                    raise ValueError("Output WAV is truncated")
                view = memoryview(data).cast("h")
                for channel, (reader, length) in enumerate(zip(readers, lengths)):
                    wanted = min(count, max(0, length - offset))
                    expected = reader.readframes(wanted) + bytes((count - wanted) * 2)
                    if view[channel::2].tobytes() != expected:
                        raise ValueError(f"Output channel {channel + 1} differs from input")

    if before != [snapshot(path) for path in sources]:
        raise ValueError("An input changed during conversion; keeping only the partial output")
    # Windows rename fails if the target appeared meanwhile; never truncate it.
    partial.rename(output)
    return {
        "output": str(output),
        "sample_rate": rate,
        "bits_per_sample": 16,
        "channels": 2,
        "frames": frames,
        "seconds": frames / rate,
        "bitrate": rate * 16 * 2,
        "padding_frames": [frames - length for length in lengths],
        "bytes": output.stat().st_size,
        "verified": "Every channel sample matches its original; padding is zero",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    directory = args.directory.resolve(strict=True)
    pairs = []
    for mic in sorted(directory.glob("*.mic.wav")):
        system = mic.with_name(mic.name.removesuffix(".mic.wav") + ".system.wav")
        if not system.is_file():
            raise FileNotFoundError(f"Missing system track for {mic}")
        pairs.append((mic, system))
    if not pairs:
        raise FileNotFoundError("No legacy WAV pairs found")
    for mic, system in pairs:
        print(json.dumps(merge_pair(mic, system), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
