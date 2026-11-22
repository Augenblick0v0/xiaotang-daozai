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
            self.assertLessEqual(len(segment), 36, segment)

    def test_english_still_splits_on_dot_question_and_exclamation(self):
        sentences, remainder = _split_sentences(
            "Hello world. This is a test! Is it working?"
        )
        self.assertEqual(
            sentences, ["Hello world.", "This is a test!", "Is it working?"]
        )
        self.assertEqual(remainder, "")

    def test_chinese_commas_are_used_for_secondary_splits(self):
        text = "今天天气很好，我们一起去公园散步，然后再去吃火锅，顺便买点水果和饮料回家"
        segments = _segments(text)
        self.assertGreaterEqual(len(segments), 2)
        self.assertEqual("".join(segments), text)
        for segment in segments:
            self.assertLessEqual(len(segment), 36, segment)

    def test_unfinished_tail_is_returned_as_remainder(self):
        sentences, remainder = _split_sentences("第一句。第二句")
        self.assertEqual(sentences, ["第一句。"])
        self.assertEqual(remainder, "第二句")


class PromptEchoTests(unittest.TestCase):
    def test_prompt_echo_is_dropped(self):
        event = to_caption_event(_front("请使用简体中文。"), 1, False)
        self.assertEqual(event["lines"], [])

    def test_real_sentence_mentioning_the_phrase_is_kept(self):
        text = "他在会上提醒大家，字幕设置里可以打开翻译，而不是一直说请使用简体中文。"
        event = to_caption_event(_front(text), 1, False)
        self.assertEqual(len(event["lines"]), 1)


class CaptionIdTests(unittest.TestCase):
    def test_ids_stay_stable_while_text_grows(self):
        full = "这是一个没有标点符号的中文长句子我们用它来测试分段逻辑是否正确以及每一段的长度是否都在限制范围之内并且编号保持不变"
        first = to_caption_event(_front(full[:40]), 1, False)
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


class CjkContinuationTest(unittest.TestCase):
    def test_stray_full_stop_before_a_particle_is_glued_back(self):
        rows, rest = _split_sentences("今天我们要讨论新版本。的发布计划以及下个季度的时间安排。")
        self.assertEqual(rest, "")
        self.assertEqual(rows, ["今天我们要讨论新版本的发布计划以及下个季度的时间安排。"])


if __name__ == "__main__":
    unittest.main()

