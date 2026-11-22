"""Device selection and WhisperLiveKit/faster-whisper patches.

WhisperLiveKit defaults to ``device="auto"``.  On a machine that has an NVIDIA
driver but no cuBLAS/cuDNN on the DLL search path, CTranslate2 picks CUDA and
then dies during warmup.  This module adds the pip-installed NVIDIA DLL
directories, probes CUDA once, and installs a small monkeypatch that pins the
device and the inference flags we want (beam size, no cross-window context).

Only the public functions are meant to be used by ``server.py``:

    configure_cuda_dlls()
    pick_device("auto")
    install_faster_whisper_patch(device, compute_type, cache_dir, beam_size)
"""
from __future__ import annotations

import os
import sys
import threading
from pathlib import Path
from typing import Optional, Tuple

# ``os.add_dll_directory`` returns an opaque handle.  The directory is removed
# from the DLL search path as soon as the handle is garbage-collected, so keep
# every handle alive for the lifetime of the process.
_DLL_HANDLES: list = []

# probe result cache: (model_size_or_path, cache_dir, device, compute_type) ->
# WhisperModel.  Reusing the probe model in the patched loader avoids loading
# the same weights twice during server startup.
_PROBE_LOCK = threading.Lock()
_PROBE_MODELS: dict = {}


def _site_packages() -> Path:
    """Return the site-packages directory of the *running* interpreter."""
    return Path(sys.prefix) / "Lib" / "site-packages"


def configure_cuda_dlls() -> list[str]:
    """Add ``site-packages/nvidia/*/bin`` to the DLL search path and ``PATH``.

    Returns the list of directories that were added.  Safe to call repeatedly;
    duplicate PATH entries are not inserted.
    """
    root = _site_packages() / "nvidia"
    if not root.is_dir():
        return []

    added: list[str] = []
    path_entries = [p for p in os.environ.get("PATH", "").split(os.pathsep) if p]
    lowered = {p.lower() for p in path_entries}

    for bin_dir in sorted(root.glob("*/bin")):
        if not bin_dir.is_dir():
            continue
        directory = str(bin_dir)
        try:
            if hasattr(os, "add_dll_directory"):
                _DLL_HANDLES.append(os.add_dll_directory(directory))
        except OSError:
            # A broken/partial nvidia package shouldn't stop the CPU fallback.
            continue
        if directory.lower() not in lowered:
            os.environ["PATH"] = directory + os.pathsep + os.environ.get("PATH", "")
            lowered.add(directory.lower())
        added.append(directory)
    return added


def _run_silence_probe(model, seconds: float = 1.0) -> None:
    """Force one real inference so lazy CUDA failures surface here."""
    import numpy as np

    audio = np.zeros(int(16000 * seconds), dtype=np.float32)
    segments, _info = model.transcribe(
        audio,
        language="en",
        beam_size=1,
        word_timestamps=False,
        condition_on_previous_text=False,
    )
    # ``transcribe`` returns a lazy generator; consume it to run the decoder.
    list(segments)


def pick_device(
    prefer: str = "auto",
    model_size: str = "base",
    cache_dir: Optional[str] = None,
) -> Tuple[str, str]:
    """Pick ``(device, compute_type)`` for faster-whisper.

    ``prefer="cpu"`` always returns ``("cpu", "int8")``.
    ``prefer="auto"`` first tries CUDA with float16 and a one-second silence
    transcription probe; any failure (no driver, missing cuBLAS, OOM, ...)
    falls back to CPU int8.
    """
    prefer = (prefer or "auto").strip().lower()
    if prefer == "cpu":
        return "cpu", "int8"

    try:
        configure_cuda_dlls()
        from faster_whisper import WhisperModel

        model = WhisperModel(
            model_size,
            device="cuda",
            compute_type="float16",
            download_root=cache_dir,
        )
        _run_silence_probe(model, seconds=1.0)
        with _PROBE_LOCK:
            _PROBE_MODELS[(str(model_size), str(cache_dir or ""), "cuda", "float16")] = model
        return "cuda", "float16"
    except Exception as exc:  # noqa: BLE001 - probe must never abort startup
        print(
            f"[wlk_loader] CUDA probe failed: {type(exc).__name__}: {exc}; "
            "falling back to cpu/int8",
            file=sys.stderr,
            flush=True,
        )
        return "cpu", "int8"


def install_faster_whisper_patch(
    device: str,
    compute_type: str,
    cache_dir: Optional[str] = None,
    beam_size: int = 1,
) -> None:
    """Monkeypatch ``FasterWhisperASR`` to use our device and decode flags.

    The original implementation hardcodes ``device="auto"``, ``beam_size=5``
    and ``condition_on_previous_text=True``.  Replacing both methods keeps the
    class contract (``self.model``, ``self.original_language``,
    ``self.transcribe_kargs``) intact while making decoding deterministic and
    cheap enough for live captions.
    """
    from whisperlivekit.local_agreement import backends as wb
    from whisperlivekit.model_paths import resolve_model_path

    def load_model(self, model_size=None, cache_dir=None, model_dir=None):
        from faster_whisper import WhisperModel

        if model_dir is not None:
            model_size_or_path = str(resolve_model_path(model_dir))
        elif model_size is not None:
            model_size_or_path = str(model_size)
        else:
            raise ValueError("Either model_size or model_dir must be set")

        key = (model_size_or_path, str(cache_dir or ""), device, compute_type)
        model = _PROBE_MODELS.get(key)
        if model is None:
            model = WhisperModel(
                model_size_or_path,
                device=device,
                compute_type=compute_type,
                download_root=cache_dir,
            )
            with _PROBE_LOCK:
                _PROBE_MODELS[key] = model

        # Keep the attribute names that the rest of WhisperLiveKit expects.
        self.model_size = model_size
        self.model_size_or_path = model_size_or_path
        self.model_dir = model_dir
        return model

    def transcribe(self, audio, init_prompt: str = "") -> list:
        options = dict(self.transcribe_kargs)
        # These are ours; never let a stale kwarg override them.
        options.pop("beam_size", None)
        options.pop("condition_on_previous_text", None)
        segments, info = self.model.transcribe(
            audio,
            language=self.original_language,
            initial_prompt=init_prompt,
            beam_size=beam_size,
            word_timestamps=True,
            condition_on_previous_text=False,
            **options,
        )
        # WhisperLiveKit's faster-whisper adapter drops ``info`` and therefore
        # never exposes the detected language.  Keep it on the instance so
        # ``ts_words`` can tag every token (and thus every Segment).
        self.detected_language = getattr(info, "language", None) or None
        self.detected_language_probability = float(
            getattr(info, "language_probability", 0.0) or 0.0
        )
        return list(segments)

    def ts_words(self, segments) -> list:
        """Same as the original, but tag tokens with the detected language."""
        tokens = []
        detected = getattr(self, "detected_language", None)
        for segment in segments:
            if segment.no_speech_prob > 0.9:
                continue
            for word in segment.words:
                tokens.append(
                    wb.ASRToken(
                        word.start,
                        word.end,
                        word.word,
                        probability=word.probability,
                        detected_language=detected,
                    )
                )
        return tokens

    wb.FasterWhisperASR.load_model = load_model
    wb.FasterWhisperASR.transcribe = transcribe
    wb.FasterWhisperASR.ts_words = ts_words
