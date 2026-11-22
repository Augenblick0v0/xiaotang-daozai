"""Convert WhisperLiveKit ``FrontData.to_dict()`` snapshots to our protocol.

The Rust client consumes a small, stable schema instead of WhisperLiveKit's
internal one.  Line ids must stay identical across snapshots so the client can
merge late translations by id.
"""
from __future__ import annotations

import hashlib
import re
from typing import Any, Optional


def parse_timestamp(value: Any) -> float:
    """Parse WhisperLiveKit's ``H:MM:SS.cc`` into seconds.

    The library formats centiseconds (``0:01:02.34``), but callers occasionally
    pass milliseconds or plain floats, so accept all of those too.
    """
    if value is None:
        return 0.0
    if isinstance(value, (int, float)):
        return float(value)

    text = str(value).strip()
    if not text:
        return 0.0

    parts = text.split(":")
    try:
        if len(parts) == 3:
            hours, minutes, seconds = parts
        elif len(parts) == 2:
            hours, minutes, seconds = "0", parts[0], parts[1]
        else:
            return float(text)

        fraction = 0.0
        if "." in seconds:
            whole, frac_text = seconds.split(".", 1)
            if frac_text:
                fraction = float("0." + frac_text)
        else:
            whole = seconds

        return int(hours) * 3600 + int(minutes) * 60 + int(whole) + fraction
    except (TypeError, ValueError):
        return 0.0


def _translation_text(value: Any) -> str:
    """WLK may serialize a translation as str/dict/dataclass; normalise it."""
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    if isinstance(value, dict):
        return str(value.get("text") or "")
    return str(getattr(value, "text", "") or "")


_SENTENCE_END_RE = re.compile(r"[.!?。！？]\s*$")
_SENTENCE_RE = re.compile(r"[^.!?。！？]+[.!?。！？]+")


def _split_sentences(text: str) -> tuple[list[str], str]:
    """Split a growing segment into display-sized sentences.

    WhisperLiveKit re-sends the whole segment on every snapshot, so a minute of
    speech would arrive as one ever-growing row that cannot fit the island
    panel.  Each chunk id keeps its ordinal suffix, so ids stay stable while
    the segment grows and late translations can still merge by id.
    """
    sentences: list[str] = []
    cursor = 0
    for match in _SENTENCE_RE.finditer(text):
        chunk = match.group().strip()
        if chunk:
            sentences.append(chunk)
        cursor = match.end()
    return sentences, text[cursor:].strip()


def _line_id(index: int, start: float) -> str:
    """Stable id: WLK line index + line start time.

    WhisperLiveKit's local-agreement output grows a line in place (the whole
    segment text is re-sent on every snapshot), so hashing the text would give
    a new id for every partial result.  Index + start stays constant for the
    lifetime of the segment while still distinguishing a reused index.
    """
    return f"L{index:04d}-{int(round(start * 1000)):08d}"


_CONTINUATION_RE = re.compile(r"^[a-z]")


def _merge_continuations(lines: list[dict]) -> list[dict]:
    """Join rows that start mid-sentence onto the row before them.

    WhisperLiveKit trims its audio buffer at segment boundaries, so a long
    sentence is often emitted in two rows: ``…we will discuss.`` followed by
    ``the new release plan…``.  Shown verbatim the island looks broken (each
    row is rendered on its own), so lowercase continuations are folded back
    into the previous row.  The dropped full stop is restored when the merged
    row is later split again by the rendering side.
    """
    merged: list[dict] = []
    for line in lines:
        text = str(line.get("text") or "")
        previous = merged[-1] if merged else None
        if previous is not None and _CONTINUATION_RE.match(text):
            head = str(previous.get("text") or "").rstrip()
            if head and head[-1] in ".!?。！？":
                head = head[:-1]
            previous["text"] = f"{head} {text}".strip()
            previous["end"] = line.get("end", previous.get("end"))
            previous["final"] = bool(previous.get("final")) and bool(line.get("final"))
            # 文本变了，旧译文作废，等 worker 按新文本重译。
            previous["translation"] = ""
            continue
        merged.append(dict(line))
    return merged



def _dedupe_lines(lines: list[dict]) -> list[dict]:
    """Keep one row per distinct text, keeping the newest copy's position.

    WhisperLiveKit re-transcribes after trimming its buffer, so the same
    sentence can show up again with a fresh start time.  Two rows with the same
    words on the island look like a glitch, so collapse them and cap the list.
    """
    seen: set[str] = set()
    out: list[dict] = []
    for line in reversed(lines):
        key = " ".join(str(line.get("text") or "").split()).lower()
        if not key or key in seen:
            continue
        seen.add(key)
        out.append(line)
    out.reverse()
    return out[-24:]


def to_caption_event(    front_dict: dict,
    seq: int,
    translate_enabled: bool,
    state: Optional[dict] = None,
) -> dict:
    """Build one ``{"type": "captions", ...}`` frame.

    ``state`` is a per-connection dict used to carry the last detected language
    across snapshots that don't contain a ``detected_language`` field.
    """
    if state is None:
        state = {}

    raw_lines = front_dict.get("lines") or []
    lines: list[dict] = []
    current_lang = str(state.get("last_lang") or "")

    stream_done = bool(state.get("stream_done"))
    line_count = len(raw_lines)

    for index, line in enumerate(raw_lines):
        if not isinstance(line, dict):
            continue
        text = str(line.get("text") or "").strip()
        if not text:
            continue

        detected = str(line.get("detected_language") or "").strip()
        if detected:
            current_lang = detected

        start = parse_timestamp(line.get("start"))
        end = parse_timestamp(line.get("end"))

        translation = ""
        if translate_enabled:
            translation = _translation_text(line.get("translation"))

        # WLK has no explicit ``final`` flag.  A line is safe to translate when
        # it is no longer the growing tail, ends with sentence punctuation, or
        # the stream has ended (which flushes the last segment).
        is_tail = index == line_count - 1
        base_final = stream_done or not is_tail

        sentences, remainder = _split_sentences(text)
        chunks = sentences + ([remainder] if remainder else [])
        if not chunks:
            continue

        total_chars = sum(len(chunk) for chunk in chunks) or 1
        consumed = 0
        span = max(end - start, 0.0)
        base_id = _line_id(index, start)

        for position, chunk in enumerate(chunks):
            chunk_start = start + span * consumed / total_chars
            consumed += len(chunk)
            chunk_end = start + span * consumed / total_chars
            # Completed sentences are final while the segment keeps growing;
            # only the trailing remainder waits for the segment to close.
            chunk_final = position < len(sentences) or base_final

            lines.append(
                {
                    "id": f"{base_id}-{position:02d}-{hashlib.sha1(chunk.encode('utf-8')).hexdigest()[:8]}",
                    "text": chunk,
                    "translation": translation if position == len(chunks) - 1 else "",
                    "lang": detected or current_lang,
                    "start": round(chunk_start, 2),
                    "end": round(chunk_end, 2),
                    "final": chunk_final,
                }
            )

    top_lang = current_lang
    if top_lang:
        state["last_lang"] = top_lang

    lines = _dedupe_lines(_merge_continuations(lines))

    lang_prob = front_dict.get("lang_prob")
    try:
        lang_prob = float(lang_prob) if lang_prob is not None else 0.0
    except (TypeError, ValueError):
        lang_prob = 0.0

    return {
        "type": "captions",
        "seq": int(seq),
        "status": str(front_dict.get("status") or ""),
        "lang": top_lang,
        "lang_prob": lang_prob,
        "lines": lines,
        "partial": str(front_dict.get("buffer_transcription") or ""),
        "buffer_translation": str(front_dict.get("buffer_translation") or ""),
    }


