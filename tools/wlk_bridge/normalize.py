"""Convert WhisperLiveKit ``FrontData.to_dict()`` snapshots to our protocol.

The Rust client consumes a small, stable schema instead of WhisperLiveKit's
internal one.  Line ids must stay identical across snapshots so the client can
merge late translations by id.
"""
from __future__ import annotations

import re
import unicodedata
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


# Three-pass split for live subtitles:
#   1. sentence punctuation (。！？!?…；; plus "." for English)
#   2. commas when a fragment is still wider than 18 CJK characters
#   3. hard wrap at 24 CJK characters / 48 Latin characters, preferring
#      punctuation or a word boundary as the cut point.
_SENTENCE_MARKS = frozenset("。！？!?…；;")
_CLAUSE_MARKS = frozenset("，,、:：")
_CLOSERS = frozenset("”’\"'」』）】〕》〉")
_SOFT_BREAKS = _SENTENCE_MARKS | _CLAUSE_MARKS | frozenset(" \t\n\r\u3000-—")

# Widths are counted in half-width cells (CJK/full-width = 2 cells), so one
# pair of thresholds works for mixed text: 36 cells = 18 CJK characters for
# the comma pass, 48 cells = 24 CJK characters or 48 Latin characters for the
# hard wrap.
_CLAUSE_MAX_WIDTH = 36
_WRAP_MAX_WIDTH = 48


def _char_width(char: str) -> int:
    return 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1


def _text_width(text: str) -> int:
    return sum(_char_width(char) for char in text)


def _is_punctuation_only(text: str) -> bool:
    """True for a fragment made only of breaks / closing quotes."""
    return bool(text) and all(
        char in _SOFT_BREAKS or char in _CLOSERS or char.isspace() for char in text
    )


def _ends_with_dot(text: str, index: int) -> bool:
    """True when the "." at *index* ends a sentence rather than a number."""
    previous = text[index - 1] if index > 0 else ""
    following = text[index + 1] if index + 1 < len(text) else ""
    # Keep decimals / version numbers together; every other "." keeps the
    # historical English behaviour of ending a sentence.
    return not (previous.isdigit() and following.isdigit())


def _consume_tail(text: str, index: int) -> int:
    """Consume a run of terminal punctuation and closing quotes."""
    end = index + 1
    while end < len(text) and (
        text[end] in _SENTENCE_MARKS or text[end] == "." or text[end] in _CLOSERS
    ):
        end += 1
    return end


def _sentence_fragments(text: str) -> list[tuple[str, bool]]:
    """Level 1: split on sentence punctuation, keeping the punctuation.

    The last item is flagged ``False`` when it is the still-growing tail.
    """
    fragments: list[tuple[str, bool]] = []
    start = 0
    index = 0
    while index < len(text):
        char = text[index]
        ends = _ends_with_dot(text, index) if char == "." else char in _SENTENCE_MARKS
        if not ends:
            index += 1
            continue
        end = _consume_tail(text, index)
        fragment = text[start:end].strip()
        if fragment:
            fragments.append((fragment, True))
        start = end
        index = end

    remainder = text[start:].strip()
    if remainder:
        fragments.append((remainder, False))
    return fragments


def _split_clauses(fragment: str) -> list[str]:
    """Level 2: split an over-long fragment on commas / enumeration marks."""
    if _text_width(fragment) <= _CLAUSE_MAX_WIDTH:
        return [fragment]

    pieces: list[str] = []
    start = 0
    for index, char in enumerate(fragment):
        if char in _CLAUSE_MARKS:
            piece = fragment[start : index + 1].strip()
            if piece:
                if pieces and _is_punctuation_only(piece):
                    pieces[-1] += piece
                else:
                    pieces.append(piece)
            start = index + 1
    tail = fragment[start:].strip()
    if tail:
        if pieces and _is_punctuation_only(tail):
            pieces[-1] += tail
        else:
            pieces.append(tail)
    return pieces or [fragment]


def _prefix_at_width(text: str, limit: int) -> int:
    """Number of characters from *text* that fit into *limit* cells."""
    width = 0
    for index, char in enumerate(text):
        char_width = _char_width(char)
        if width + char_width > limit:
            return index
        width += char_width
    return len(text)


def _soft_break_index(text: str, cut: int) -> Optional[int]:
    """Last punctuation/space cut in the second half of ``text[:cut]``."""
    floor = max(1, cut // 2)
    for index in range(cut - 1, floor - 1, -1):
        if text[index] in _SOFT_BREAKS:
            return index + 1
    return None


def _hard_split(fragment: str) -> list[str]:
    """Level 3: wrap at the width limit, preferring punctuation / spaces."""
    pieces: list[str] = []
    rest = fragment
    while rest:
        # 拉丁字母比汉字窄一半，按同一宽度切会把英文句子切成两段；
        # 英文放宽到 96 个字符，尽量保持一句一行。
        limit = 96 if rest.isascii() else _WRAP_MAX_WIDTH
        if _text_width(rest) <= limit:
            break
        cut = _prefix_at_width(rest, limit)
        if cut <= 0:
            cut = 1
        preferred = _soft_break_index(rest, cut)
        if preferred is not None:
            cut = preferred
        piece = rest[:cut].strip()
        if piece:
            pieces.append(piece)
        rest = rest[cut:].lstrip()
    if rest.strip():
        tail = rest.strip()
        if pieces and _is_punctuation_only(tail):
            # Keep a lone "。" / "，" attached to the text it closes instead of
            # emitting a punctuation-only subtitle row.
            pieces[-1] += tail
        else:
            pieces.append(tail)
    return pieces


def _join(left: str, right: str) -> str:
    """Join two fragments without gluing English words together."""
    if left and right and left[-1].isascii() and right[0].isascii():
        return f"{left} {right}"
    return f"{left}{right}"


def _split_sentences(text: str) -> tuple[list[str], str]:
    """Split a growing segment into display-sized rows.

    WhisperLiveKit re-sends the whole segment on every snapshot, so a minute of
    speech would arrive as one ever-growing row that cannot fit the island
    panel.  Returns ``(stable_rows, growing_remainder)``: rows that will no
    longer change go in the first list, while only the last hard-wrapped piece
    of an unfinished fragment is returned as the remainder.  Splitting is
    greedy from the left, which keeps earlier chunks (and therefore their
    positional ids) stable while the segment grows.
    """
    text = str(text or "")
    if not text.strip():
        return [], ""

    sentences: list[str] = []
    remainder = ""
    for fragment, complete in _sentence_fragments(text):
        pieces: list[str] = []
        for clause in _split_clauses(fragment):
            pieces.extend(_hard_split(clause))
        pieces = [piece for piece in pieces if piece]
        if not pieces:
            continue
        if complete:
            sentences.extend(pieces)
        else:
            # The tail is still being spoken: everything before it is stable.
            sentences.extend(pieces[:-1])
            remainder = pieces[-1]
    # Whisper 常在停顿处塞一个句号，把一句话拆成两行；后一行若以小写开头，
    # 说明它是同一句的续写，先合回去再上屏。
    joined: list[str] = []
    for piece in sentences:
        if joined and _CONTINUATION_RE.match(piece) and joined[-1][-1:] in ".!?。！？":
            joined[-1] = f"{joined[-1].rstrip('.!?。！？')} {piece}"
        else:
            joined.append(piece)
    return joined, remainder


def _line_id(index: int, start: float) -> str:
    """Stable id: WLK line index + line start time.

    WhisperLiveKit's local-agreement output grows a line in place (the whole
    segment text is re-sent on every snapshot), so hashing the text would give
    a new id for every partial result.  Index + start stays constant for the
    lifetime of the segment while still distinguishing a reused index.
    """
    return f"L{index:04d}-{int(round(start * 1000)):08d}"


_CONTINUATION_RE = re.compile(r"^[a-z]")

# Whisper 的中文输出经常是繁体（训练数据里繁体占很大比例）。用户要的是简体，
# 在生成字幕行之前统一转一次；没装 opencc 时原样放行，别因为一个可选依赖把整条链路搞断。
_OPENCC = None
_OPENCC_FAILED = False


def _to_simplified(text: str, lang: str) -> str:
    global _OPENCC, _OPENCC_FAILED
    if not text or not str(lang or "").lower().startswith("zh"):
        return text
    if _OPENCC is None and not _OPENCC_FAILED:
        try:
            from opencc import OpenCC  # type: ignore[import-not-found]

            _OPENCC = OpenCC("t2s")
        except Exception:
            _OPENCC_FAILED = True
    if _OPENCC is None:
        return text
    try:
        return _OPENCC.convert(text)
    except Exception:
        return text



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


def _merge_short_fragments(lines: list[dict]) -> list[dict]:
    """Fold tiny ASR fragments into the row before them.

    WhisperLiveKit re-decodes a rolling buffer, so Chinese output often arrives
    as ``来到产品会。`` + ``议。`` instead of one sentence.  Rows shorter than
    10 characters carry no useful meaning on their own; glue them onto the
    previous row until it grows past a comfortable subtitle width.
    """
    merged: list[dict] = []
    for line in lines:
        text = str(line.get("text") or "").strip()
        if not text:
            continue
        if merged:
            previous = merged[-1]
            head = str(previous.get("text") or "").rstrip()
            joined = _join(head.rstrip(".!?。！？"), text)
            if (
                head[-1:] in ".!?。！？"
                and _visible_len(text) < 10
                and _visible_len(joined) <= 24
            ):
                previous["text"] = joined
                previous["end"] = line.get("end", previous.get("end"))
                previous["final"] = bool(previous.get("final")) and bool(line.get("final"))
                previous["translation"] = ""
                continue
        merged.append(dict(line))
    return merged


def _visible_len(text: str) -> int:
    return len("".join(str(text).split()))


def _dedupe_lines(lines: list[dict]) -> list[dict]:
    """Collapse duplicate rows, preferring line id over text.

    WhisperLiveKit re-transcribes after trimming its buffer, so the same
    sentence can show up again with a fresh start time; two rows with the same
    words on the island look like a glitch.  Ids are checked first: a line that
    grows in place keeps its id, so the last (most complete) copy replaces the
    older one instead of both being kept or the new text being dropped.  Rows
    left after that are de-duplicated by normalised text.  The newest copy's
    position is kept and the list is capped so a long session cannot grow the
    frame without bound.
    """
    by_id: dict[str, dict] = {}
    order: list[str] = []
    without_id: list[dict] = []
    for line in lines:
        line_id = str(line.get("id") or "")
        if not line_id:
            without_id.append(line)
            continue
        if line_id not in by_id:
            order.append(line_id)
        by_id[line_id] = line

    candidates = [by_id[line_id] for line_id in order] + without_id

    seen_text: set[str] = set()
    out: list[dict] = []
    for line in reversed(candidates):
        text = " ".join(str(line.get("text") or "").split()).lower()
        if not text or text in seen_text:
            continue
        seen_text.add(text)
        out.append(line)
    out.reverse()
    return out[-64:]


def to_caption_event(
    front_dict: dict,
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
    # Fold WLK's buffer-boundary continuations *before* splitting: doing it
    # after would merge our own 48-character Latin wraps back together.
    raw_lines = _merge_continuations(
        [line for line in raw_lines if isinstance(line, dict)]
    )
    lines: list[dict] = []
    current_lang = str(state.get("last_lang") or "")

    stream_done = bool(state.get("stream_done"))
    line_count = len(raw_lines)

    for index, line in enumerate(raw_lines):
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
                    # Positional id only: the text keeps growing while WLK
                    # re-sends the segment, so hashing it would break merging.
                    "id": f"{base_id}-{position:02d}",
                    "text": _to_simplified(chunk, detected or current_lang),
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

    lines = _dedupe_lines(_merge_short_fragments(lines))

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
        "partial": _to_simplified(str(front_dict.get("buffer_transcription") or ""), top_lang),
        "buffer_translation": str(front_dict.get("buffer_translation") or ""),
    }


