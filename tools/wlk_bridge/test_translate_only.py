"""Tests for the sidecar's ``--translate-only`` mode.

Run either way (no NLLB/Whisper model is ever loaded):

    python -m pytest tools/wlk_bridge/test_translate_only.py -q
    python tools/wlk_bridge/test_translate_only.py
"""
from __future__ import annotations

import pathlib
import sys
import threading

HERE = pathlib.Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from fastapi.testclient import TestClient  # noqa: E402

import server  # noqa: E402


class FakeWorker:
    """Stand-in for ``TranslationWorker``: records jobs, answers immediately."""

    def __init__(self, ready: bool = True, result: str | None = None) -> None:
        self.ready = ready
        self.last_error = ""
        self.result = result
        self.jobs: list[tuple[str, str, str, str]] = []
        self._results: dict[str, str] = {}
        self._lock = threading.Lock()

    def preload(self) -> None:
        self.ready = True

    def submit(self, line_id: str, text: str, src: str, tgt: str) -> bool:
        with self._lock:
            self.jobs.append((line_id, text, src, tgt))
            self._results[line_id] = self.result if self.result is not None else f"[{tgt}] {text}"
        return True

    def take(self, line_id: str) -> str | None:
        with self._lock:
            return self._results.pop(line_id, None)

    def close(self, timeout: float = 0.0) -> None:
        pass


class RejectingWorker(FakeWorker):
    """Mimics a worker that cannot accept jobs (e.g. unsupported language)."""

    def __init__(self) -> None:
        super().__init__()
        self.last_error = "model offline"

    def submit(self, line_id: str, text: str, src: str, tgt: str) -> bool:
        return False


class SilentWorker(FakeWorker):
    """Accepts jobs but never produces a result, to exercise the timeout."""

    def take(self, line_id: str) -> str | None:
        return None


def make_client(worker: FakeWorker | None = None, translate_only: bool = True):
    argv = ["--port", "0"]
    if translate_only:
        argv.append("--translate-only")
    args = server.parse_args(argv)
    bridge = server.Bridge(
        args, "cpu", None, worker or FakeWorker(), translate_only=translate_only
    )
    return TestClient(server.create_app(bridge)), bridge


def test_health_translate_only_flags() -> None:
    client, bridge = make_client()
    response = client.get("/health")
    assert response.status_code == 200
    assert response.json() == {
        "status": "ok",
        "asr_ready": False,
        "translate_ready": True,
        "translate_only": True,
    }
    assert bridge.translate_only is True

    cold_client, _cold = make_client(FakeWorker(ready=False))
    assert cold_client.get("/health").json()["translate_ready"] is False


def test_health_asr_mode_keeps_asr_fields() -> None:
    client, _bridge = make_client(translate_only=False)
    data = client.get("/health").json()
    assert data["asr_ready"] is True
    assert data["device"] == "cpu"
    assert data["model"] == "base"


def test_translate_returns_worker_result() -> None:
    worker = FakeWorker()
    client, _bridge = make_client(worker)
    response = client.post("/translate", json={"text": "hello", "src": "en", "tgt": "zh"})
    assert response.status_code == 200
    assert response.json() == {"translation": "[zh] hello"}
    assert len(worker.jobs) == 1
    line_id, text, src, tgt = worker.jobs[0]
    assert line_id.startswith("http-")
    assert (text, src, tgt) == ("hello", "en", "zh")


def test_translate_empty_text_skips_worker() -> None:
    worker = FakeWorker()
    client, _bridge = make_client(worker)
    response = client.post("/translate", json={"text": "", "src": "en", "tgt": "zh"})
    assert response.status_code == 200
    assert response.json() == {"translation": ""}
    assert worker.jobs == []


def test_translate_same_language_returns_original() -> None:
    worker = FakeWorker()
    client, _bridge = make_client(worker)
    response = client.post("/translate", json={"text": "你好", "src": "zh-CN", "tgt": "zh"})
    assert response.status_code == 200
    assert response.json() == {"translation": "你好"}
    assert worker.jobs == []


def test_translate_uses_unique_line_ids() -> None:
    worker = FakeWorker()
    client, _bridge = make_client(worker)
    for _ in range(2):
        assert client.post(
            "/translate", json={"text": "same", "src": "en", "tgt": "zh"}
        ).status_code == 200
    ids = [job[0] for job in worker.jobs]
    assert len(ids) == 2
    assert ids[0] != ids[1]


def test_translate_rejected_job_returns_502() -> None:
    client, _bridge = make_client(RejectingWorker())
    response = client.post("/translate", json={"text": "hello", "src": "en", "tgt": "zh"})
    assert response.status_code == 502
    assert response.json()["error"] == "model offline"


def test_translate_timeout_returns_502() -> None:
    client, _bridge = make_client(SilentWorker())
    original = server.TRANSLATE_HTTP_TIMEOUT
    server.TRANSLATE_HTTP_TIMEOUT = 0.05
    try:
        response = client.post(
            "/translate", json={"text": "hello", "src": "en", "tgt": "zh"}
        )
    finally:
        server.TRANSLATE_HTTP_TIMEOUT = original
    assert response.status_code == 502
    assert "timed out" in response.json()["error"]


def test_asr_returns_503_in_translate_only() -> None:
    client, _bridge = make_client()
    response = client.get("/asr")
    assert response.status_code == 503
    assert response.json() == {"type": "error", "error": "translation-only mode"}


def test_asr_ws_rejects_translate_only() -> None:
    from starlette.websockets import WebSocketDisconnect

    client, _bridge = make_client()
    with client.websocket_connect("/asr") as websocket:
        assert websocket.receive_json() == {
            "type": "error",
            "error": "translation-only mode",
        }
        try:
            websocket.receive_json()
        except WebSocketDisconnect:
            pass


def test_main_translate_only_skips_asr_startup() -> None:
    """`--translate-only` must construct the worker and never touch ASR code."""
    created: list[FakeWorker] = []

    class Worker(FakeWorker):
        def __init__(self, cache_dir, nllb_size="600M") -> None:  # noqa: ANN001
            super().__init__(ready=False)
            self.cache_dir = cache_dir
            self.preloaded = False
            self.closed = False
            created.append(self)

        def preload(self) -> None:
            self.preloaded = True

        def close(self, timeout: float = 0.0) -> None:
            self.closed = True

    seen: dict[str, object] = {}

    async def fake_serve(args, bridge) -> None:  # noqa: ANN001
        seen["bridge"] = bridge

    def asr_path(*_args, **_kwargs):
        raise AssertionError("ASR startup must not run in translate-only mode")

    originals = (
        server.TranslationWorker,
        server.serve_forever,
        server.configure_cuda_dlls,
        server.pick_device,
        server.build_engine,
    )
    server.TranslationWorker = Worker
    server.serve_forever = fake_serve
    server.configure_cuda_dlls = asr_path
    server.pick_device = asr_path
    server.build_engine = asr_path
    try:
        assert server.main(["--translate-only", "--port", "0"]) == 0
    finally:
        (
            server.TranslationWorker,
            server.serve_forever,
            server.configure_cuda_dlls,
            server.pick_device,
            server.build_engine,
        ) = originals

    assert len(created) == 1
    worker = created[0]
    assert worker.preloaded is True
    assert worker.closed is True
    bridge = seen["bridge"]
    assert bridge.translate_only is True
    assert bridge.engine is None
    assert bridge.device == "cpu"


def _main() -> int:
    tests = [(name, value) for name, value in sorted(globals().items()) if name.startswith("test_") and callable(value)]
    failures = 0
    for name, test in tests:
        try:
            test()
        except Exception as exc:  # noqa: BLE001 - report every failing test
            failures += 1
            print(f"FAIL {name}: {type(exc).__name__}: {exc}")
        else:
            print(f"PASS {name}")
    print(f"{len(tests) - failures}/{len(tests)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(_main())
