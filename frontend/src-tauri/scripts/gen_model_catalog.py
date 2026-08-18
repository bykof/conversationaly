#!/usr/bin/env python3
"""Regenerate TRANSCRIBE_MODEL_CATALOG in src/config.rs from transcribe.cpp model cards.

The cards under transcribe.cpp's docs/models/ carry a Download table with the
exact HF filename, byte size, and (usually) a measured WER per quantization.
That is every column the catalog needs, so the catalog is generated rather than
hand-typed for ~50 variants.

Usage:
    python3 scripts/gen_model_catalog.py [path/to/transcribe.cpp/docs/models]

With no argument it resolves the cargo git checkout of the pinned rev. Rewrites
only the region between the GENERATED markers in src/config.rs.
"""

import glob
import json
import re
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CONFIG_RS = REPO_ROOT / "src" / "config.rs"
BEGIN = "    // BEGIN GENERATED — see scripts/gen_model_catalog.py\n"
END = "    // END GENERATED\n"

# Variants deliberately kept out of the catalog.
EXCLUDED = {
    # Speaker diarizer: emits speaker turns, never text. Cannot be a
    # transcription model.
    "diar_streaming_sortformer_4spk-v2.1",
    # Upstream google/medasr is gated behind the Health AI Developer
    # Foundations terms, so the download 401s for most users.
    "medasr",
    # 24B. Even Q4_K_M is far past what a meeting app should pull down.
    "voxtral-small-24b-2507",
}

# Streaming-capable variants, for the pre-download "live-capable" label only.
#
# ponytail: hand-maintained because the cards do not mark this reliably —
# nemotron-3.5 and voxtral-realtime have no "## Streaming" section, and
# parakeet-unified-en-0.6b is a unified streaming/offline model whose name says
# nothing. Runtime dispatch reads Capabilities::supports_streaming from the GGUF
# instead, so a stale entry here mislabels a row but cannot pick the wrong
# decode path. Recheck against the family table in transcribe.cpp's README when
# bumping the pinned rev.
STREAMING = {
    "moonshine-streaming-tiny",
    "moonshine-streaming-small",
    "moonshine-streaming-medium",
    "nemotron-speech-streaming-en-0.6b",
    "nemotron-3.5-asr-streaming-0.6b",
    "multitalker-parakeet-streaming-0.6b-v1",
    "parakeet-unified-en-0.6b",
    "voxtral-mini-4b-realtime-2602",
}

# (variant prefix, display family, languages, accuracy when the card's Download
# table has no WER column). Longest prefix wins, so order matters.
FAMILIES = [
    ("moonshine-streaming-", "Moonshine Streaming", "English only", "Good"),
    ("moonshine-", "Moonshine", "English only", "Good"),
    ("nemotron-3.5-asr-streaming-", "Nemotron 3.5", "Multilingual — 39 locales", "High"),
    ("nemotron-speech-streaming-", "Nemotron Speech", "English only", "High"),
    ("multitalker-parakeet-streaming-", "Multitalker Parakeet", "English only", "Good"),
    ("parakeet-tdt-0.6b-v3", "Parakeet TDT v3", "Multilingual — 25 European languages", "High"),
    ("parakeet-unified-en-", "Parakeet Unified", "English only", "High"),
    ("parakeet-", "Parakeet", "English only", "High"),
    ("canary-1b-v2", "Canary v2", "Multilingual — 25 European languages", "High"),
    ("canary-qwen-", "Canary-Qwen", "English only", "High"),
    ("canary-", "Canary", "Multilingual — 4 languages", "High"),
    ("whisper-", "Whisper", "Multilingual — 99 languages", "Good"),
    ("gigaam-", "GigaAM", "Russian", "Good"),
    ("qwen3-asr-", "Qwen3-ASR", "Multilingual — 30 languages", "High"),
    ("cohere-transcribe-", "Cohere Transcribe", "Multilingual — 14 languages", "High"),
    ("sensevoice-", "SenseVoice", "Multilingual — 5 languages", "Good"),
    ("fun-asr-mlt-nano-", "FunASR Nano MLT", "Multilingual — 31 languages", "Good"),
    ("fun-asr-nano-", "FunASR Nano", "Chinese and English", "Good"),
    ("granite-", "Granite Speech", "Multilingual", "Good"),
    ("voxtral-mini-4b-realtime-", "Voxtral Realtime", "Multilingual", "Good"),
    ("voxtral-", "Voxtral", "Multilingual — 8 languages", "Good"),
    ("moss-transcribe-diarize", "MOSS Transcribe-Diarize", "English and Chinese", "Good"),
    ("sensevoicesmall", "SenseVoice", "Multilingual — 5 languages", "Good"),
]

# Quantization suffix -> catalog name suffix. Q8_0 is the default download;
# Q4_K_M is offered as well once Q8_0 grows past this many MB.
Q8, Q4 = "Q8_0", "Q4_K_M"
Q4_THRESHOLD_MB = 500

# The filename comes from the URL, not the link text: most cards use the
# filename as link text but voxtral-realtime.md just says "GGUF".
ROW = re.compile(
    r"^\|\s*(?P<quant>[A-Za-z0-9_]+)\s*\|\s*\[[^\]]+\]"
    r"\(https://huggingface\.co/[^/]+/(?P<repo>[^/]+)/resolve/[^)]*?/"
    r"(?P<file>[^/)]+\.gguf)\)\s*\|"
    r"\s*(?P<size>[\d.]+)\s*(?P<unit>MB|GB)\s*\|"
    r"(?:\s*(?P<wer>[\d.]+)%\s*\|)?"
)


def family_of(variant):
    for prefix, name, languages, fallback in FAMILIES:
        if variant.startswith(prefix):
            # Whisper's .en siblings are English-only regardless of the family
            # default; every other family is uniform across its variants.
            if variant.endswith(".en"):
                languages = "English only"
            return name, languages, fallback
    raise SystemExit(f"no family mapping for variant {variant!r} — add one to FAMILIES")


def accuracy_of(wer, fallback):
    """Map measured WER to the three tiers the UI renders as icons.

    Cutoffs come from the actual spread across all cards (n=370 quant rows:
    p25 1.63%, median 2.07%, p75 3.33%, long tail to 26.8%). Textbook-looking
    thresholds like 5%/10% would label ~85% of the catalog "High" and make the
    icon meaningless.

    ponytail: WER is not comparable across families — GigaAM's number is on a
    Russian set, FunASR's on Chinese, Whisper's on LibriSpeech test-clean. Good
    enough to sort a model into three buckets, not to rank two models against
    each other.
    """
    if wer is None:
        return fallback
    if wer < 2.0:
        return "High"
    if wer < 4.5:
        return "Good"
    return "Decent"


def speed_of(size_mb, streaming):
    """ponytail: size stands in for parameter count, which stands in for speed.

    The cards publish per-backend latency tables but no single comparable RTF,
    and this only feeds a three-word label.
    """
    # Streaming models are real-time native by construction — but only the small
    # ones. voxtral-mini-4b-realtime is streaming and 4.8 GB at Q8; calling that
    # "Very Fast" would be a lie, so size still gets the last word above 1 GB.
    if streaming and size_mb < 1000:
        return "Very Fast"
    if size_mb < 150:
        return "Very Fast"
    if size_mb < 400:
        return "Fast"
    if size_mb < 1000:
        return "Medium"
    return "Slow"


def wer_set_of(text):
    """The eval set the card's WER column is measured on.

    Every card measures LibriSpeech test-clean except GigaAM's four, which are
    Russian models scored on FLEURS ru. One keyword rather than a hand-kept list
    so it cannot drift from the card.
    """
    if "FLEURS ru test split" in text:
        return "FLEURS ru (Russian)"
    return "LibriSpeech test-clean"


def wer_from_preset_table(text, quants):
    """{quant: wer} from a `| Preset | … | LibriSpeech test-clean … |` table.

    Some cards (nemotron-3.5, the default model) keep WER in a per-preset
    accuracy table instead of the Download table. Without this the default row
    would be the only blank one in the picker. Rows are accepted only for quants
    the Download table already listed, so an unrelated table cannot leak in.
    """
    column = None
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if column is None:
            for i, cell in enumerate(cells):
                if "LibriSpeech test-clean" in cell:
                    column = i
            continue
        if cells[0] in quants and len(cells) > column:
            try:
                out[cells[0]] = float(cells[column])
            except ValueError:
                continue
    return out


def parse_card(path):
    """Return {quant: (slug, repo, filename, size_mb, wer, wer_set)} for one card.

    `slug` is the lowercased variant name and is what the catalog stores;
    `filename` keeps the repo's real casing (Fun-ASR-Nano-2512, SenseVoiceSmall)
    because it has to match the URL byte for byte.
    """
    text = path.read_text(encoding="utf-8")
    quants = {}
    for line in text.splitlines():
        m = ROW.match(line.strip())
        if not m:
            continue
        size = float(m["size"])
        size_mb = round(size * 1024) if m["unit"] == "GB" else round(size)
        slug = m["file"][: -len(f"-{m['quant']}.gguf")].lower()
        quants[m["quant"]] = (
            slug,
            m["repo"],
            m["file"],
            size_mb,
            float(m["wer"]) if m["wer"] else None,
            wer_set_of(text),
        )

    if quants and all(q[4] is None for q in quants.values()):
        fallback = wer_from_preset_table(text, quants)
        for quant, wer in fallback.items():
            *head, _, _ = quants[quant]
            # The fallback reads the LibriSpeech column by name, so the set is
            # that column regardless of what else the card benchmarks.
            quants[quant] = (*head, wer, "LibriSpeech test-clean")
    return quants


GGUF_KV_TYPE_SIZES = {0: 1, 1: 1, 2: 2, 3: 2, 4: 4, 5: 4, 6: 4, 7: 1, 10: 8, 11: 8, 12: 8}

# Enough of every GGUF seen so far to cover the whole KV header. The language
# list sits in it, long before any tensor data.
GGUF_HEADER_BYTES = 400_000

LANG_CACHE = Path(tempfile.gettempdir()) / "transcribe-catalog-languages.json"


def gguf_languages(repo, filename):
    """`general.languages` straight out of the model's GGUF header.

    The model's own answer, not a blurb someone typed: the hand-kept table this
    replaced claimed 39 locales for nemotron-3.5 (its GGUF says 32) and a bare
    "Multilingual" for Granite. Only the header is fetched — a few hundred KB by
    HTTP range, not the multi-gigabyte weights.
    """
    cache = json.loads(LANG_CACHE.read_text()) if LANG_CACHE.exists() else {}
    if filename in cache:
        return cache[filename]

    url = f"https://huggingface.co/handy-computer/{repo}/resolve/main/{filename}"
    with tempfile.NamedTemporaryFile() as tmp:
        got = subprocess.run(
            ["curl", "-sL", "--max-time", "120", "-r", f"0-{GGUF_HEADER_BYTES}",
             "-o", tmp.name, url]
        )
        if got.returncode != 0:
            raise SystemExit(f"could not fetch GGUF header for {filename}")
        langs = _parse_gguf_languages(Path(tmp.name))

    if langs is None:
        raise SystemExit(f"{filename} has no general.languages — cannot label it honestly")

    cache[filename] = langs
    LANG_CACHE.write_text(json.dumps(cache))
    return langs


def _parse_gguf_languages(path):
    with path.open("rb") as f:
        if f.read(4) != b"GGUF":
            raise SystemExit(f"{path} is not a GGUF file")
        f.read(4)   # version
        f.read(8)   # tensor count
        n_kv = struct.unpack("<Q", f.read(8))[0]

        def read_str():
            n = struct.unpack("<Q", f.read(8))[0]
            return f.read(n).decode("utf-8", "replace")

        for _ in range(n_kv):
            key = read_str()
            kind = struct.unpack("<I", f.read(4))[0]
            if kind == 8:
                value = read_str()
            elif kind == 9:
                elem = struct.unpack("<I", f.read(4))[0]
                count = struct.unpack("<Q", f.read(8))[0]
                if elem == 8:
                    value = [read_str() for _ in range(count)]
                else:
                    f.read(GGUF_KV_TYPE_SIZES.get(elem, 4) * count)
                    value = None
            else:
                f.read(GGUF_KV_TYPE_SIZES.get(kind, 4))
                value = None
            if key == "general.languages":
                return value
    return None


def entries_for(quants):
    """Pick the quantizations to ship for one variant."""
    if Q8 not in quants:
        return []
    slug, _, _, size_mb, _, _ = quants[Q8]
    if slug in EXCLUDED:
        return []

    family, _, fallback = family_of(slug)
    streaming = slug in STREAMING
    picked = [("q8", quants[Q8])]
    if size_mb > Q4_THRESHOLD_MB and Q4 in quants:
        picked.append(("q4", quants[Q4]))

    out = []
    for suffix, (_, repo, filename, size_mb, wer, wer_set) in picked:
        # No languages here — that has its own field, and repeating it produced
        # "Nemotron 3.5 — Multilingual — 39 locales".
        note = " — smaller download, lower memory use" if suffix == "q4" else ""
        out.append(
            {
                "name": f"{slug}-{suffix}",
                "family": family,
                "hf_repo": repo,
                "filename": filename,
                "size_mb": size_mb,
                "accuracy": accuracy_of(wer, fallback),
                "wer": wer,
                "wer_set": wer_set if wer is not None else "",
                "speed": speed_of(size_mb, streaming),
                "streaming": streaming,
                "languages": gguf_languages(repo, filename),
                "description": f"{family}{note}",
            }
        )
    return out


def render(entries):
    lines = [BEGIN]
    for e in entries:
        lines.append("    TranscribeModel {\n")
        lines.append(f'        name: "{e["name"]}",\n')
        lines.append(f'        family: "{e["family"]}",\n')
        lines.append(f'        hf_repo: "{e["hf_repo"]}",\n')
        lines.append(f'        filename: "{e["filename"]}",\n')
        lines.append(f'        size_mb: {e["size_mb"]},\n')
        lines.append(f'        accuracy: "{e["accuracy"]}",\n')
        wer = "None" if e["wer"] is None else f'Some({e["wer"]:.2f})'
        lines.append(f"        wer: {wer},\n")
        lines.append(f'        wer_set: "{e["wer_set"]}",\n')
        lines.append(f'        speed: "{e["speed"]}",\n')
        lines.append(f'        streaming: {"true" if e["streaming"] else "false"},\n')
        codes = ", ".join(f'"{c}"' for c in e["languages"])
        lines.append(f"        languages: &[{codes}],\n")
        lines.append(f'        description: "{e["description"]}",\n')
        lines.append("    },\n")
    lines.append(END)
    return "".join(lines)


def default_docs_dir():
    hits = glob.glob(
        str(Path.home() / ".cargo/git/checkouts/transcribe.cpp-*/*/docs/models")
    )
    if not hits:
        raise SystemExit(
            "no transcribe.cpp checkout found — pass docs/models explicitly "
            "(run `cargo fetch` first)"
        )
    return Path(sorted(hits)[-1])


def main():
    docs = Path(sys.argv[1]) if len(sys.argv) > 1 else default_docs_dir()
    cards = sorted(docs.glob("*.md"))
    if not cards:
        raise SystemExit(f"no model cards under {docs}")

    entries = []
    for card in cards:
        entries.extend(entries_for(parse_card(card)))
    # Recommended-first ordering is a UI concern; keep the file deterministic.
    entries.sort(key=lambda e: (not e["streaming"], e["name"]))

    source = CONFIG_RS.read_text(encoding="utf-8")
    start, end = source.index(BEGIN), source.index(END) + len(END)
    CONFIG_RS.write_text(source[:start] + render(entries) + source[end:], encoding="utf-8")

    live = sum(1 for e in entries if e["streaming"])
    print(f"{len(entries)} entries written to {CONFIG_RS} ({live} live-capable)")
    print(f"skipped by name: {', '.join(sorted(EXCLUDED))}")


if __name__ == "__main__":
    main()
