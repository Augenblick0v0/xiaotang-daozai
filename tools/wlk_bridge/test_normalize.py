"""Unit tests for the WLK bridge caption normalizer.

Pure stdlib ``unittest``: no model, no network, no WebSocket server.  Run from
the repository root::

    D:\\_research\\wlk-venv\\Scripts\\python.exe -m unittest tools.wlk_bridge.test_normalize -v
"""
from __future__ import annotations

import unittest

from tools.wlk_bridge.normalize import _dedupe_lines, _split_sentences, to_caption_event
from tools.wlk_bridge.server import (
    DEFAULT_ZH_CONTEXT,
    MAX_CONTEXT_CHARS,
    resolve_context,
    resolve_translation,
)


def _segments(text: str) -> list[str]:
    """The display rows ``to_caption_event`` would emit for *text*."""
    sentences, remainder = _split_sentences(text)
    return sentences + ([remainder] if remainder else [])


def _front(
    text: str,
    start: str = "0:00:00.00",
    end: str = "0:00:06.00",
) -> dict:
    return {
        "lines": [
            {
                "text": text,
                "start": start,
                "end": end,
                "detected_language": "zh",
            }
        ]
    }


class SplitSentencesTests(unittest.TestCase):
    def test_chinese_without_punctuation_is_split_into_short_rows(self):
        text = "这是一个没有标点符号的中文长句子我们用它来测试分段逻辑是否正确并且确保每一段都不会太长"
        self.assertGreaterEqual(len(text), 40)
        segments = _segments(text)
        self.assertGreaterEqual(len(segments), 2)
        self.assertEqual("".join(segments), text)
        for segment in segments:
            self.assertLessEqual(len(segment), 24, segment)

    def test_english_still_splits_on_dot_question_and_exclamation(self):
        sentences, remainder = _split_sentences(
            "Hello world. This is a test! Is it working?"
        )
        self.assertEqual(
            sentences, ["Hello world.", "This is a test!", "Is it working?"]
        )
        self.assertEqual(remainder, "")

    def test_chinese_commas_are_used_for_secondary_splits(self):
        text = "今天天气很好，我们一起去公园散步，然后再去吃火锅"
        segments = _segments(text)
        self.assertGreaterEqual(len(segments), 3)
        self.assertEqual("".join(segments), text)
        for segment in segments:
            self.assertLessEqual(len(segment), 24, segment)

    def test_unfinished_tail_is_returned_as_remainder(self):
        sentences, remainder = _split_sentences("第一句。第二句")
        self.assertEqual(sentences, ["第一句。"])
        self.assertEqual(remainder, "第二句")


class CaptionIdTests(unittest.TestCase):
    def test_ids_stay_stable_while_text_grows(self):
        full = "这是一个没有标点符号的中文长句子我们用它来测试分段逻辑是否正确"
        first = to_caption_event(_front(full[:30]), 1, False)
        second = to_caption_event(_front(full), 2, False)
        first_ids = [line["id"] for line in first["lines"]]
        second_ids = [line["id"] for line in second["lines"]]
        self.assertGreaterEqual(len(first_ids), 2)
        self.assertGreaterEqual(len(second_ids), len(first_ids))
        self.assertEqual(first_ids, second_ids[: len(first_ids)])
        # No text digest: WLK line id + position only.
        self.assertEqual(first_ids[0], "L0000-00000000-00")

    def test_dedupe_prefers_id_and_keeps_grown_text(self):
        lines = [
            {"id": "L0000-00000000-00", "text": "你好"},
            {"id": "L0000-00000000-01", "text": "你好"},
            {"id": "L0000-00000000-00", "text": "你好，世界"},
        ]
        deduped = _dedupe_lines(lines)
        texts = [line["text"] for line in deduped]
        self.assertIn("你好，世界", texts)
        self.assertEqual(sum(1 for text in texts if text == "你好"), 1)


class TranslationDecisionTests(unittest.TestCase):
    def test_same_language_skips_translation(self):
        self.assertEqual(
            resolve_translation("zh", "zh-CN", True), (False, "same_language")
        )
        self.assertEqual(
            resolve_translation("en-US", "en", True), (False, "same_language")
        )
        self.assertEqual(
            resolve_translation("zh-CN", "zh_TW", True), (False, "same_language")
        )

    def test_different_language_keeps_translation(self):
        self.assertEqual(resolve_translation("zh", "en", True), (True, ""))
        self.assertEqual(resolve_translation("auto", "zh", True), (True, ""))

    def test_translation_not_requested_is_disabled(self):
        self.assertEqual(resolve_translation("zh", "en", False), (False, ""))

    def test_zh_gets_default_context_and_long_context_is_truncated(self):
        self.assertEqual(resolve_context("zh-CN", ""), DEFAULT_ZH_CONTEXT)
        self.assertEqual(resolve_context("en-US", ""), "")
        long_context = "提示词" * 40
        self.assertEqual(
            len(resolve_context("zh", long_context)), MAX_CONTEXT_CHARS
        )
        self.assertEqual(
            resolve_context("en", long_context), long_context[:MAX_CONTEXT_CHARS]
        )


if __name__ == "__main__":
    unittest.main()
