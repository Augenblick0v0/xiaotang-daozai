"""WhisperLiveKit bridge sidecar for 岛仔 live captions.

Run:
    python tools/wlk_bridge/server.py --port 8125 --model base --device auto
    python tools/wlk_bridge/server.py --port 8125 --translate-only

Protocol (WebSocket ``/asr``):
    client -> binary PCM frames, 16 kHz mono s16le
    server -> config, captions (<= 5 fps, only on change), translation,
              error, ready_to_stop

HTTP endpoints (same host/port, FastAPI + uvicorn):
    GET  /health     -> readiness flags
    POST /translate  -> one-shot NLLB translation

``--translate-only`` skips WhisperLiveKit entirely (no ``TranscriptionEngine``,
no ``AudioProcessor``, no model load) and only runs the NLLB worker.
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import logging
import os
import sys
import time
import uuid
from types import SimpleNamespace
from typing import Any, Optional
from urllib.parse import parse_qs, urlparse

import uvicorn
import websockets
from fastapi import FastAPI, WebSocket
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.websockets import WebSocketDisconnect

try:  # ``python server.py``
    from normalize import to_caption_event
    from translate_worker import TranslationWorker
    from wlk_loader import configure_cuda_dlls, install_faster_whisper_patch, pick_device
except ImportError:  # ``python -m wlk_bridge.server``
    from .normalize import to_caption_event
    from .translate_worker import TranslationWorker
    from .wlk_loader import configure_cuda_dlls, install_faster_whisper_patch, pick_device

DEFAULT_CACHE_DIR = os.path.join(os.environ.get("LOCALAPPDATA") or os.path.expanduser("~/.cache"), "daozai", "wlk", "models")
CAPTION_MIN_INTERVAL = 0.2  # 5 frames / second at most
TRANSLATION_WAIT_SECONDS = 120.0
# ``POST /translate`` is a synchronous request/response call; keep it short so
# a stalled model load surfaces as 502 instead of hanging the caller.
TRANSLATE_HTTP_TIMEOUT = 30.0
TRANSLATE_POLL_INTERVAL = 0.02

LOG = logging.getLogger("wlk_bridge")


def log(message: str) -> None:
    print(f"[wlk_bridge] {message}", flush=True)


def _query_value(query: dict, name: str, default: str = "") -> str:
    values = query.get(name)
    return values[0] if values else default


def _as_bool(value: str) -> bool:
    return str(value).strip().lower() in {"1", "true", "yes", "on"}


# 不再给解码器塞提示词：Whisper 会把提示词本身当成音频内容转录出来，
# 视频里的音乐/音效尤其容易触发。简体化交给 normalize 的 OpenCC 转换。
DEFAULT_ZH_CONTEXT = ""
MAX_CONTEXT_CHARS = 60


def _language_base(code: str) -> str:
    """Return the primary language subtag: ``zh-CN`` / ``zh_TW`` -> ``zh``."""
    text = str(code or "").strip().lower().replace("_", "-")
    if not text or text == "auto":
        return ""
    return text.split("-", 1)[0]


def resolve_translation(
    language: str, target_language: str, translate_requested: bool
) -> tuple[bool, str]:
    """Decide whether translation is needed.

    Returns ``(translate_enabled, skip_reason)``.  A same-language request
    (zh->zh, en-US->en) is treated as "no translation" instead of asking NLLB
    to translate a sentence into its own language.
    """
    if not translate_requested or not str(target_language or "").strip():
        return False, ""
    source_base = _language_base(language)
    target_base = _language_base(target_language)
    if source_base and source_base == target_base:
        return False, "same_language"
    return True, ""


def resolve_context(language: str, raw_context: str) -> str:
    """Pick the decoder prompt: explicit query value first, zh gets a default."""
    context = str(raw_context or "").strip()
    return context[:MAX_CONTEXT_CHARS]


def build_engine(args: argparse.Namespace):
    from whisperlivekit.config import WhisperLiveKitConfig
    from whisperlivekit.core import TranscriptionEngine

    config = WhisperLiveKitConfig(
        backend="faster-whisper",
        backend_policy=args.policy,
        model_size=args.model,
        model_cache_dir=args.cache_dir,
        lan="auto",
        pcm_input=True,
        vac=True,
        vad=True,
        min_chunk_size=args.min_chunk,
        log_level="WARNING",
    )
    return TranscriptionEngine(config=config)


def warmup_engine(engine) -> None:
    """Force one inference so a broken CUDA install fails before WLK_READY."""
    import numpy as np

    audio = np.zeros(16000, dtype=np.float32)
    engine.asr.transcribe(audio)


class TranslationRejected(RuntimeError):
    """The worker refused the job (empty/auto language or a trimmed queue)."""


class TranslationTimeout(RuntimeError):
    """No translation arrived before the HTTP deadline."""


def wait_for_translation(
    translator: Any,
    line_id: str,
    text: str,
    src: str,
    tgt: str,
    timeout: float = TRANSLATE_HTTP_TIMEOUT,
) -> str:
    """Blocking submit/poll loop; call it through ``asyncio.to_thread``.

    ``TranslationWorker`` is a single background thread, so polling here only
    waits on that thread; the event loop stays free for other requests.
    """
    if not translator.submit(line_id, text, src, tgt):
        detail = getattr(translator, "last_error", "") or "translation worker rejected the request"
        raise TranslationRejected(detail)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = translator.take(line_id)
        if value is not None:
            return value
        time.sleep(TRANSLATE_POLL_INTERVAL)
    raise TranslationTimeout(f"translation timed out after {timeout:g}s")


class _WebSocketConnection:
    """Give Starlette's WebSocket the surface ``Bridge._handle`` expects.

    ``Bridge`` was written against ``websockets``; this adapter keeps the
    protocol code untouched while uvicorn owns the socket.
    """

    def __init__(self, websocket: WebSocket) -> None:
        self._websocket = websocket
        query = websocket.url.query
        path = websocket.url.path + (f"?{query}" if query else "")
        self.request = SimpleNamespace(path=path)

    async def send(self, data: Any) -> None:
        if isinstance(data, (bytes, bytearray, memoryview)):
            await self._websocket.send_bytes(bytes(data))
        else:
            await self._websocket.send_text(str(data))

    def __aiter__(self) -> "_WebSocketConnection":
        return self

    async def __anext__(self) -> Any:
        message = await self._websocket.receive()
        if message["type"] == "websocket.disconnect":
            raise WebSocketDisconnect(message.get("code") or 1000)
        payload = message.get("bytes")
        if payload is not None:
            return payload
        return message.get("text") or ""


class TranslateRequest(BaseModel):
    text: str
    src: str
    tgt: str


class Bridge:
    def __init__(
        self,
        args: argparse.Namespace,
        device: str,
        engine,
        translator: TranslationWorker,
        translate_only: bool = False,
    ):
        self.args = args
        self.device = device
        self.engine = engine
        self.translator = translator
        self.translate_only = translate_only


    # --------------------------------------------------------------- WS part
    async def handler(self, websocket) -> None:
        await self._handle(websocket)

    async def _handle(self, websocket) -> None:
        from whisperlivekit.audio_processor import AudioProcessor

        request = getattr(websocket, "request", None)
        path = request.path if request is not None else "/asr"
        query = parse_qs(urlparse(path).query)

        language = _query_value(query, "language", "auto") or "auto"
        target_language = _query_value(query, "target_language", "")
        translate_requested = _as_bool(_query_value(query, "translate", "0"))
        translate_enabled, translate_skip_reason = resolve_translation(
            language, target_language, translate_requested
        )
        context = resolve_context(language, _query_value(query, "context", ""))

        session_id = os.urandom(4).hex()
        log(
            f"session {session_id}: language={language!r} "
            f"target={target_language!r} translate={translate_enabled} "
            f"context_chars={len(context)}"
        )
        if translate_skip_reason:
            log(f"session {session_id}: translation skipped ({translate_skip_reason})")
        if translate_enabled:
            # Load NLLB while audio is streaming; captions never wait for it.
            self.translator.preload()

        ap = AudioProcessor(
            transcription_engine=self.engine,
            language=language,
            mode="full",
            target_language=None,
            context=context or None,
        )

        pump: Optional[asyncio.Task] = None
        try:
            config_frame = {
                "type": "config",
                "sample_rate": 16000,
                "channels": 1,
                "format": "s16le",
                "mode": "full",
                "translate": translate_enabled,
                "translate_ready": self.translator.ready,
                "translate_skipped": bool(translate_skip_reason),
            }
            if translate_skip_reason:
                config_frame["reason"] = translate_skip_reason
            await websocket.send(
                json.dumps(config_frame, ensure_ascii=False)
            )

            pump = asyncio.create_task(
                self._pump(
                    websocket,
                    ap,
                    session_id=session_id,
                    target_language=target_language if translate_enabled else "",
                    fallback_language=language,
                ),
                name=f"wlk-pump-{session_id}",
            )

            async for message in websocket:
                if isinstance(message, (bytes, bytearray, memoryview)):
                    payload = bytes(message)
                    if not payload:
                        break
                    await ap.process_audio(payload)
                else:
                    # Optional text control frames; the Rust side normally just
                    # closes the socket or sends an empty binary frame.
                    try:
                        control = json.loads(message)
                    except (TypeError, ValueError):
                        continue
                    if str(control.get("type", "")).lower() in {"stop", "end", "eof"}:
                        break

            await ap.process_audio(None)
            if pump is not None:
                try:
                    await asyncio.wait_for(
                        asyncio.shield(pump), timeout=TRANSLATION_WAIT_SECONDS + 10
                    )
                except asyncio.TimeoutError:
                    log(f"session {session_id}: pump timed out")
                except asyncio.CancelledError:
                    raise
                except Exception as exc:  # noqa: BLE001
                    log(f"session {session_id}: pump ended with {type(exc).__name__}: {exc}")
        except (websockets.ConnectionClosed, WebSocketDisconnect):
            log(f"session {session_id}: client disconnected")
        except Exception as exc:  # noqa: BLE001 - report and clean up
            LOG.exception("session %s failed", session_id)
            await self._try_send_error(websocket, f"{type(exc).__name__}: {exc}")
        finally:
            try:
                await ap.process_audio(None)
            except Exception:
                pass
            if pump is not None and not pump.done():
                pump.cancel()
            if pump is not None:
                try:
                    await pump
                except (asyncio.CancelledError, Exception):
                    pass
            try:
                await ap.cleanup()
            except Exception:
                LOG.exception("session %s: cleanup failed", session_id)
            try:
                await websocket.send(json.dumps({"type": "ready_to_stop"}))
            except Exception:
                pass
            log(f"session {session_id}: closed")

    async def _pump(
        self,
        websocket,
        ap,
        session_id: str,
        target_language: str,
        fallback_language: str,
    ) -> None:
        """Read FrontData snapshots and push protocol frames.

        Translation runs entirely on ``TranslationWorker``; this loop only
        polls ``take()`` and never awaits model work.
        """
        generator = await ap.create_tasks()
        queue: asyncio.Queue = asyncio.Queue(maxsize=64)

        async def produce() -> None:
            try:
                async for front in generator:
                    data = front.to_dict()
                    # FrontData has no language probability; the patched ASR
                    # keeps it on the backend instance.
                    try:
                        data["lang_prob"] = float(
                            getattr(self.engine.asr, "detected_language_probability", 0.0)
                            or 0.0
                        )
                    except (TypeError, ValueError):
                        data["lang_prob"] = 0.0
                    await queue.put(data)
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001
                try:
                    queue.put_nowait({"__error__": f"{type(exc).__name__}: {exc}"})
                except asyncio.QueueFull:
                    pass
            finally:
                try:
                    queue.put_nowait(None)
                except asyncio.QueueFull:
                    pass

        producer = asyncio.create_task(produce(), name=f"wlk-gen-{session_id}")
        loop = asyncio.get_running_loop()

        state: dict = {}
        seq = 0
        last_sent = 0.0
        last_body: Optional[str] = None
        pending_front: Optional[dict] = None
        last_front: Optional[dict] = None
        stream_done = False
        final_flush_done = False
        # line id -> text already submitted; a growing line may be translated
        # again once it has grown enough to be worth a second request.
        submitted_text: dict[str, str] = {}
        task_meta: dict[str, dict] = {}
        translations: dict[str, str] = {}
        waiting: dict[str, str] = {}  # task key -> line id
        translation_deadline: Optional[float] = None

        try:
            while True:
                if not stream_done:
                    try:
                        item = await asyncio.wait_for(queue.get(), timeout=0.05)
                    except asyncio.TimeoutError:
                        item = "__timeout__"
                    if item is None:
                        stream_done = True
                        # Re-evaluate the last snapshot with ``stream_done`` so
                        # the growing tail becomes final and can be translated.
                        if last_front is not None and not final_flush_done:
                            final_flush_done = True
                            state["stream_done"] = True
                            pending_front = last_front
                    elif item != "__timeout__":
                        if "__error__" in item:
                            await self._try_send_error(websocket, item["__error__"])
                        elif item.get("error"):
                            await self._try_send_error(websocket, str(item["error"]))
                        else:
                            pending_front = item
                            last_front = item
                else:
                    await asyncio.sleep(0.05)

                now = loop.time()

                if pending_front is not None:
                    event = to_caption_event(
                        pending_front, seq + 1, bool(target_language), state
                    )

                    if target_language:
                        for line in event["lines"]:
                            line_id = line["id"]
                            text = line["text"]
                            if not line["final"] or not text:
                                continue
                            previous = submitted_text.get(line_id)
                            if previous == text:
                                continue
                            # Translating every partial snapshot would queue
                            # dozens of nearly identical jobs; wait until the
                            # line has grown by a meaningful amount.
                            if (
                                previous is not None
                                and len(text) - len(previous) < 20
                                and not state.get("stream_done")
                            ):
                                continue
                            src = line["lang"] or event["lang"] or fallback_language
                            if not src or src == "auto":
                                continue
                            submitted_text[line_id] = text
                            digest = hashlib.blake2b(
                                text.encode("utf-8"), digest_size=4
                            ).hexdigest()
                            task_key = f"{session_id}:{line_id}:{digest}"
                            # A newer text supersedes the older pending request
                            # for the same line; its late result is ignored.
                            for old_key, old_line in list(waiting.items()):
                                if old_line == line_id:
                                    waiting.pop(old_key, None)
                            task_meta[task_key] = {
                                "id": line_id,
                                "text": text,
                                "lang": src,
                            }
                            if self.translator.submit(
                                task_key, text, src, target_language
                            ):
                                waiting[task_key] = line_id

                    for line in event["lines"]:
                        if (
                            line["id"] in translations
                            and submitted_text.get(line["id"], line["text"]) == line["text"]
                        ):
                            line["translation"] = translations[line["id"]]

                    body_obj = {key: value for key, value in event.items() if key != "seq"}
                    body = json.dumps(body_obj, ensure_ascii=False, sort_keys=True)
                    if body == last_body:
                        pending_front = None
                    elif now - last_sent >= CAPTION_MIN_INTERVAL:
                        await websocket.send(json.dumps(event, ensure_ascii=False))
                        seq += 1
                        last_sent = now
                        last_body = body
                        pending_front = None

                if target_language:
                    for task_key, line_id in list(waiting.items()):
                        value = self.translator.take(task_key)
                        if value is None:
                            continue
                        waiting.pop(task_key, None)
                        if not value:
                            continue
                        translations[line_id] = value
                        meta = task_meta.get(task_key, {})
                        await websocket.send(
                            json.dumps(
                                {
                                    "type": "translation",
                                    "id": line_id,
                                    "translation": value,
                                    "lang": meta.get("lang", ""),
                                },
                                ensure_ascii=False,
                            )
                        )

                if stream_done and pending_front is None and not waiting:
                    break
                if stream_done and waiting and translation_deadline is None:
                    translation_deadline = now + TRANSLATION_WAIT_SECONDS
                if translation_deadline is not None and now >= translation_deadline:
                    log(
                        f"session {session_id}: translation wait timed out "
                        f"({len(waiting)} line(s) pending)"
                    )
                    break
        finally:
            if not producer.done():
                producer.cancel()
            try:
                await producer
            except (asyncio.CancelledError, Exception):
                pass

    @staticmethod
    async def _try_send_error(websocket, message: str) -> None:
        try:
            await websocket.send(
                json.dumps({"type": "error", "error": message}, ensure_ascii=False)
            )
        except Exception:
            pass


def create_app(bridge: Bridge) -> FastAPI:
    """Build the HTTP + WebSocket app (used by ``serve_forever`` and tests)."""
    app = FastAPI(title="wlk_bridge", docs_url=None, redoc_url=None, openapi_url=None)

    @app.get("/health")
    @app.get("/healthz")
    async def health() -> JSONResponse:
        if bridge.translate_only:
            payload = {
                "status": "ok",
                "asr_ready": False,
                "translate_ready": bool(bridge.translator.ready),
                "translate_only": True,
            }
        else:
            payload = {
                "status": "ok",
                "asr_ready": True,
                "device": bridge.device,
                "model": bridge.args.model,
                "translate_ready": bool(bridge.translator.ready),
            }
        return JSONResponse(payload, headers={"Cache-Control": "no-store"})

    @app.api_route("/asr", methods=["GET", "POST"])
    async def asr_http() -> JSONResponse:
        if bridge.translate_only:
            return JSONResponse(
                status_code=503,
                content={"type": "error", "error": "translation-only mode"},
            )
        return JSONResponse(
            status_code=426,
            content={"type": "error", "error": "websocket upgrade required"},
        )

    @app.post("/translate")
    async def translate(payload: TranslateRequest) -> JSONResponse:
        text = payload.text or ""
        if not text.strip():
            return JSONResponse({"translation": ""})
        source = _language_base(payload.src)
        if source and source == _language_base(payload.tgt):
            return JSONResponse({"translation": text})

        # Unique id per HTTP call: the worker dedupes on (line_id, text) and
        # trims its queue, so every request must be distinguishable.
        line_id = f"http-{uuid.uuid4().hex}"
        try:
            translation = await asyncio.to_thread(
                wait_for_translation,
                bridge.translator,
                line_id,
                text,
                payload.src,
                payload.tgt,
                TRANSLATE_HTTP_TIMEOUT,
            )
        except TranslationTimeout as exc:
            return JSONResponse(status_code=502, content={"error": str(exc)})
        except TranslationRejected as exc:
            return JSONResponse(status_code=502, content={"error": str(exc)})
        if not translation:
            detail = (
                getattr(bridge.translator, "last_error", "")
                or "translation worker returned an empty result"
            )
            return JSONResponse(status_code=502, content={"error": detail})
        return JSONResponse({"translation": translation})

    @app.websocket("/asr")
    async def asr_websocket(websocket: WebSocket) -> None:
        await websocket.accept()
        if bridge.translate_only:
            await websocket.send_text(
                json.dumps(
                    {"type": "error", "error": "translation-only mode"},
                    ensure_ascii=False,
                )
            )
            await websocket.close(code=1011)
            return
        await bridge.handler(_WebSocketConnection(websocket))

    return app


async def serve_forever(args: argparse.Namespace, bridge: Bridge) -> None:
    config = uvicorn.Config(
        create_app(bridge),
        host=args.host,
        port=args.port,
        log_level="warning",
        access_log=False,
        lifespan="off",
    )
    server = uvicorn.Server(config)
    task = asyncio.create_task(server.serve(), name="wlk-uvicorn")
    # uvicorn prints its own banner at info level; wait for the bind so the
    # WLK_READY line carries the real port when --port 0 was requested.
    while not getattr(server, "started", False):
        if task.done():
            await task
            raise RuntimeError("uvicorn stopped before startup completed")
        await asyncio.sleep(0.02)

    port = args.port
    for listener in getattr(server, "servers", None) or []:
        sockets = getattr(listener, "sockets", None) or []
        if sockets:
            try:
                port = sockets[0].getsockname()[1]
            except OSError:
                port = args.port
            break

    if bridge.translate_only:
        ready = {
            "port": port,
            "asr_ready": False,
            "translate_only": True,
            "device": "cpu",
            "model": "nllb",
        }
    else:
        ready = {
            "port": port,
            "asr_ready": True,
            "device": bridge.device,
            "model": args.model,
        }
    print(
        "WLK_READY " + json.dumps(ready, ensure_ascii=False, separators=(",", ":")),
        flush=True,
    )

    try:
        await task
    finally:
        server.should_exit = True


def parse_args(argv: Optional[list[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="WhisperLiveKit bridge for 岛仔")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8125)
    parser.add_argument("--model", default="base")
    parser.add_argument("--device", default="auto", choices=["auto", "cpu", "cuda"])
    parser.add_argument("--beam", type=int, default=1)
    parser.add_argument("--min-chunk", type=float, default=1.0)
    parser.add_argument("--policy", default="localagreement", choices=["localagreement", "1", "2"])
    parser.add_argument("--cache-dir", default=DEFAULT_CACHE_DIR)
    parser.add_argument("--nllb-size", default="600M")
    parser.add_argument("--translation-wait", type=float, default=TRANSLATION_WAIT_SECONDS)
    parser.add_argument(
        "--preload-translation",
        action="store_true",
        help="warm up NLLB in the background at startup so the first caption translates fast",
    )
    parser.add_argument(
        "--translate-only",
        action="store_true",
        help="skip WhisperLiveKit entirely (no ASR model); serve /health and POST /translate only",
    )
    return parser.parse_args(argv)


def main(argv: Optional[list[str]] = None) -> int:
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stderr.reconfigure(encoding="utf-8")

    args = parse_args(argv)
    global TRANSLATION_WAIT_SECONDS
    TRANSLATION_WAIT_SECONDS = args.translation_wait

    os.environ["HF_HOME"] = args.cache_dir
    os.environ.setdefault("HF_HUB_CACHE", os.path.join(args.cache_dir, "hub"))
    logging.basicConfig(level=logging.WARNING, format="[wlk %(levelname)s] %(message)s")

    if args.translate_only:
        # No WhisperLiveKit import, patch or model load here: this mode only
        # serves NLLB for callers that already have their own captions.
        log("translate-only mode: ASR disabled, NLLB only")
        translator = TranslationWorker(args.cache_dir, nllb_size=args.nllb_size)
        log("preloading translation model ...")
        translator.preload()
        bridge = Bridge(args, "cpu", None, translator, translate_only=True)
        try:
            asyncio.run(serve_forever(args, bridge))
        except KeyboardInterrupt:
            log("interrupted")
        finally:
            translator.close()
        return 0

    configure_cuda_dlls()
    device, compute_type = pick_device(
        args.device, model_size=args.model, cache_dir=args.cache_dir
    )
    log(f"device={device} compute_type={compute_type} model={args.model}")

    install_faster_whisper_patch(
        device, compute_type, cache_dir=args.cache_dir, beam_size=args.beam
    )

    log("loading ASR model ...")
    try:
        engine = build_engine(args)
        log("warming up ASR ...")
        warmup_engine(engine)
    except Exception as exc:  # noqa: BLE001 - report, then try the always-cached base model
        log(f"model {args.model} failed: {exc!r}")
        if args.model != "base":
            log("falling back to base ...")
            args.model = "base"
            engine = build_engine(args)
            warmup_engine(engine)
        else:
            raise
    log("ASR ready")

    translator = TranslationWorker(args.cache_dir, nllb_size=args.nllb_size)
    if args.preload_translation:
        log("preloading translation model ...")
        translator.preload()
    bridge = Bridge(args, device, engine, translator)

    try:
        asyncio.run(serve_forever(args, bridge))
    except KeyboardInterrupt:
        log("interrupted")
    finally:
        translator.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())



