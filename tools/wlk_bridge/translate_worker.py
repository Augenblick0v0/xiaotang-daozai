"""Background sentence translator for the WLK bridge.

NLLB-200 is loaded lazily in a single worker thread so the websocket snapshot
loop never blocks on model loading or generation.  WhisperLiveKit's own
translation is unusable here because ``--target-language`` requires a concrete
source language while we run ``language=auto``; the worker therefore takes the
source language from each line's ``detected_language``.

The model is loaded with plain ``transformers`` (``nllw.OnlineTranslation`` is
a token-stream API and has no sentence-level ``simple_translation``).
"""
from __future__ import annotations

import importlib.util
import os
import queue
import threading
from pathlib import Path
from typing import Optional, Tuple

# Small fallback map so submit()/take() still work if nllw import fails.
_FALLBACK_NLLB = {
    "en": "eng_Latn",
    "zh": "zho_Hans",
    "zh-cn": "zho_Hans",
    "zh-tw": "zho_Hant",
    "ja": "jpn_Jpan",
    "ko": "kor_Hang",
    "fr": "fra_Latn",
    "de": "deu_Latn",
    "es": "spa_Latn",
    "pt": "por_Latn",
    "ru": "rus_Cyrl",
    "it": "ita_Latn",
    "ar": "arb_Arab",
    "hi": "hin_Deva",
    "th": "tha_Thai",
    "vi": "vie_Latn",
    "id": "ind_Latn",
    "tr": "tur_Latn",
    "nl": "nld_Latn",
    "pl": "pol_Latn",
}


_NLLW_LANGUAGES = None
_NLLW_LANGUAGES_LOADED = False


def _nllw_languages():
    """Load ``nllw/languages.py`` without importing the heavy ``nllw`` package.

    ``import nllw`` pulls in torch/transformers (tens of seconds on a cold
    process).  The language table itself is dependency-free, so load that file
    directly and still use nllw's authoritative ``get_nllb_code``.
    """
    global _NLLW_LANGUAGES, _NLLW_LANGUAGES_LOADED
    if _NLLW_LANGUAGES_LOADED:
        return _NLLW_LANGUAGES
    _NLLW_LANGUAGES_LOADED = True
    try:
        package = importlib.util.find_spec("nllw")
        if package is None or not package.submodule_search_locations:
            return None
        path = Path(list(package.submodule_search_locations)[0]) / "languages.py"
        if not path.is_file():
            return None
        spec = importlib.util.spec_from_file_location("_wlk_nllw_languages", path)
        if spec is None or spec.loader is None:
            return None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        _NLLW_LANGUAGES = module
    except Exception:
        _NLLW_LANGUAGES = None
    return _NLLW_LANGUAGES


def to_nllb_code(code: str) -> Optional[str]:
    """Convert ``en``/``zh``/``zh-CN``/... to an NLLB language tag."""
    if not code:
        return None
    code = str(code).strip()
    if not code or code.lower() == "auto":
        return None

    languages = _nllw_languages()
    if languages is not None:
        try:
            converted = (
                languages.get_nllb_code(code)
                or languages.convert_to_nllb_code(code)
            )
            if converted and converted != "auto":
                return converted
        except Exception:
            pass

    return _FALLBACK_NLLB.get(code.replace("_", "-").lower())


class TranslationWorker:
    """One background thread, one NLLB model, many queued sentences."""

    def __init__(self, src_cache_dir: str, nllb_size: str = "600M") -> None:
        self.cache_dir = str(src_cache_dir)
        self.nllb_size = str(nllb_size)
        self.model_name = f"facebook/nllb-200-distilled-{self.nllb_size}"

        # HF must see the shared cache before huggingface_hub is imported.
        os.environ["HF_HOME"] = self.cache_dir
        os.environ.setdefault("HF_HUB_CACHE", os.path.join(self.cache_dir, "hub"))
        if os.environ.get("WLK_NLLB_ALLOW_DOWNLOAD", "0") != "1":
            # Hard offline mode: even a metadata HEAD request can stall for
            # seconds when the machine has no route to huggingface.co.
            os.environ.setdefault("HF_HUB_OFFLINE", "1")
            os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")

        self._queue: "queue.Queue[Optional[Tuple[str, str, str, str]]]" = queue.Queue()
        self._lock = threading.Lock()
        self._results: dict[str, str] = {}
        # Permanent dedupe set: (line_id, text) is translated at most once.
        self._seen: set[Tuple[str, str]] = set()
        self._pending = 0
        self._ready = threading.Event()
        self._preload = threading.Event()
        self._last_error = ""
        self._stopped = False

        self._torch = None
        self._tokenizer = None
        self._model = None
        self._device = "cpu"

        self._thread = threading.Thread(
            target=self._run, name="wlk-nllb", daemon=True
        )
        self._thread.start()

    # ------------------------------------------------------------------ API
    @property
    def ready(self) -> bool:
        return self._ready.is_set()

    @property
    def last_error(self) -> str:
        return self._last_error

    def pending(self) -> int:
        """Number of submitted sentences without a result yet."""
        with self._lock:
            return self._pending

    def preload(self) -> None:
        """Start loading NLLB in the worker thread without blocking.

        Called when a session enables translation, so the model loads while
        audio is still streaming instead of after the first final sentence.
        """
        self._preload.set()

    def submit(self, line_id: str, text: str, src: str, tgt: str) -> bool:
        """Queue one sentence.  Returns False for empty/duplicate/auto jobs.

        Never blocks: the queue is unbounded and all model work happens on the
        worker thread.
        """
        text = (text or "").strip()
        src = (src or "").strip()
        tgt = (tgt or "").strip()
        if not line_id or not text or not src or not tgt:
            return False
        if src.lower() == "auto" or tgt.lower() == "auto":
            return False

        key = (line_id, text)
        with self._lock:
            if key in self._seen:
                return False
            self._seen.add(key)
            self._pending += 1

        # 屏幕上只在意最近几句，积压时丢掉最旧的，别让译文永远慢半分钟。
        while self._queue.qsize() > 4:
            try:
                self._queue.get_nowait()
            except queue.Empty:
                break
            with self._lock:
                self._pending = max(0, self._pending - 1)
        self._queue.put_nowait((line_id, text, src, tgt))
        return True

    def take(self, line_id: str) -> Optional[str]:
        """Pop a finished translation, or ``None`` if it isn't ready yet."""
        with self._lock:
            return self._results.pop(line_id, None)

    def close(self, timeout: float = 0.0) -> None:
        """Ask the worker to stop; with ``timeout=0`` it never blocks."""
        if self._stopped:
            return
        self._stopped = True
        self._queue.put_nowait(None)
        if timeout > 0:
            self._thread.join(timeout=timeout)

    # -------------------------------------------------------------- internals
    def _run(self) -> None:
        while True:
            if self._preload.is_set() and self._model is None:
                self._preload.clear()
                try:
                    self._ensure_model()
                except Exception as exc:  # noqa: BLE001 - submit() will retry
                    self._last_error = f"{type(exc).__name__}: {exc}"
            try:
                job = self._queue.get(timeout=0.2)
            except queue.Empty:
                continue
            try:
                if job is None:
                    return
                line_id, text, src, tgt = job
                result = ""
                try:
                    self._ensure_model()
                    src_code = to_nllb_code(src)
                    tgt_code = to_nllb_code(tgt)
                    if not src_code or not tgt_code:
                        raise ValueError(f"unsupported language pair: {src!r} -> {tgt!r}")
                    result = self._translate(text, src_code, tgt_code)
                except Exception as exc:  # noqa: BLE001 - one bad line must not kill the worker
                    self._last_error = f"{type(exc).__name__}: {exc}"
                    result = ""
                with self._lock:
                    self._results[line_id] = result
                    self._pending = max(0, self._pending - 1)
            finally:
                self._queue.task_done()

    def _ensure_model(self) -> None:
        if self._model is not None:
            return

        import torch
        from transformers import AutoModelForSeq2SeqLM, AutoTokenizer

        torch.set_num_threads(max(2, min(8, (os.cpu_count() or 4) - 2)))
        self._torch = torch

        # Never hit the network by default: the model is expected to be in the
        # shared cache.  Set WLK_NLLB_ALLOW_DOWNLOAD=1 to permit a download.
        local_only = os.environ.get("WLK_NLLB_ALLOW_DOWNLOAD", "0") != "1"
        kwargs = {"local_files_only": local_only}

        tokenizer = AutoTokenizer.from_pretrained(self.model_name, **kwargs)
        model = AutoModelForSeq2SeqLM.from_pretrained(self.model_name, **kwargs).eval()

        self._device = "cuda" if torch.cuda.is_available() else "cpu"
        if self._device == "cuda":
            model = model.to("cuda").half()
        else:
            model = torch.quantization.quantize_dynamic(
                model, {torch.nn.Linear}, dtype=torch.qint8
            )

        self._tokenizer = tokenizer
        self._model = model
        self._ready.set()

    def _translate(self, text: str, src_code: str, tgt_code: str) -> str:
        tokenizer = self._tokenizer
        model = self._model
        tokenizer.src_lang = src_code

        encoded = tokenizer(text, return_tensors="pt", truncation=True, max_length=512)
        if self._device == "cuda":
            encoded = {key: value.to("cuda") for key, value in encoded.items()}

        with self._torch.no_grad():
            output = model.generate(
                **encoded,
                forced_bos_token_id=tokenizer.convert_tokens_to_ids(tgt_code),
                max_new_tokens=256,
                num_beams=1,
            )
        return tokenizer.batch_decode(output, skip_special_tokens=True)[0].strip()
