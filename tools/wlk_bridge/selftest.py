"""End-to-end self test for the WLK bridge.

Usage:
    python tools/wlk_bridge/selftest.py --wav D:\\_research\\wlk-test\\en1_16k.wav --target zh

It starts ``server.py`` as a child process, waits for ``WLK_READY``, streams
the WAV as 100 ms PCM chunks over ``/asr``, and prints the recognised text,
detected language and translation.  Exit code 0 only when the assertions pass.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import subprocess
import sys
import wave
from pathlib import Path
from typing import Optional
from urllib.parse import quote

import websockets

HERE = Path(__file__).resolve().parent
SERVER = HERE / "server.py"
CJK_RE = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff]")


def parse_args(argv: Optional[list[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="WLK bridge self test")
    parser.add_argument("--wav", required=True, help="16 kHz mono s16le WAV")
    parser.add_argument("--target", default="zh", help="target language, e.g. zh")
    parser.add_argument("--port", type=int, default=8199)
    parser.add_argument("--device", default="auto", choices=["auto", "cpu", "cuda"])
    parser.add_argument("--model", default="base")
    parser.add_argument("--beam", type=int, default=1)
    parser.add_argument("--cache-dir", default=r"D:\_research\wlk-cache")
    parser.add_argument("--nllb-size", default="600M")
    parser.add_argument("--timeout", type=float, default=90.0, help="translation wait seconds")
    parser.add_argument("--python", default=sys.executable, help="python used for the server")
    return parser.parse_args(argv)


def read_wav(path: str) -> bytes:
    with wave.open(path, "rb") as wav:
        if wav.getframerate() != 16000:
            raise SystemExit(f"{path}: expected 16000 Hz, got {wav.getframerate()}")
        if wav.getnchannels() != 1:
            raise SystemExit(f"{path}: expected mono, got {wav.getnchannels()} channels")
        if wav.getsampwidth() != 2:
            raise SystemExit(f"{path}: expected 16-bit PCM, got {wav.getsampwidth() * 8}-bit")
        return wav.readframes(wav.getnframes())


async def run(args: argparse.Namespace) -> int:
    pcm = read_wav(args.wav)

    cmd = [
        args.python,
        str(SERVER),
        "--host",
        "127.0.0.1",
        "--port",
        str(args.port),
        "--model",
        args.model,
        "--device",
        args.device,
        "--beam",
        str(args.beam),
        "--cache-dir",
        args.cache_dir,
        "--nllb-size",
        args.nllb_size,
        "--translation-wait",
        str(args.timeout),
    ]

    env = dict(os.environ)
    env["PYTHONUNBUFFERED"] = "1"
    env["PYTHONIOENCODING"] = "utf-8"
    env["HF_HOME"] = args.cache_dir
    env.setdefault("HF_HUB_CACHE", os.path.join(args.cache_dir, "hub"))

    print(f"[selftest] starting: {' '.join(cmd)}", flush=True)
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
        env=env,
    )

    ready = asyncio.Event()
    ready_info: dict = {}

    async def pump_server_stdout() -> None:
        assert proc.stdout is not None
        while True:
            raw = await proc.stdout.readline()
            if not raw:
                return
            text = raw.decode("utf-8", "replace").rstrip()
            print(f"[server] {text}", flush=True)
            if text.startswith("WLK_READY "):
                try:
                    ready_info.update(json.loads(text[len("WLK_READY ") :]))
                except json.JSONDecodeError:
                    pass
                ready.set()

    stdout_task = asyncio.create_task(pump_server_stdout())

    try:
        exited = asyncio.create_task(proc.wait())
        waited = asyncio.create_task(ready.wait())
        done, pending = await asyncio.wait(
            {exited, waited}, timeout=180.0, return_when=asyncio.FIRST_COMPLETED
        )
        for task in pending:
            task.cancel()
        if not ready.is_set():
            print("[selftest] server did not become ready", flush=True)
            return 1

        port = int(ready_info.get("port") or args.port)
        url = (
            f"ws://127.0.0.1:{port}/asr"
            f"?language=auto&target_language={quote(args.target)}&translate=1"
        )
        print(f"[selftest] connecting {url}", flush=True)

        messages: list[dict] = []
        flags = {"text": False, "translation": False}

        async with websockets.connect(url, max_size=None) as ws:
            config_raw = await asyncio.wait_for(ws.recv(), timeout=15)
            print(f"[selftest] config: {config_raw}", flush=True)

            last_caption = ""

            async def reader() -> None:
                nonlocal last_caption
                async for raw in ws:
                    if isinstance(raw, (bytes, bytearray)):
                        continue
                    try:
                        message = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    messages.append(message)
                    kind = message.get("type")
                    if kind == "captions":
                        lines = [
                            line
                            for line in message.get("lines") or []
                            if str(line.get("text") or "").strip()
                        ]
                        if lines:
                            flags["text"] = True
                            current = lines[-1].get("text", "")
                            if current != last_caption:
                                last_caption = current
                                print(
                                    f"[caption] seq={message.get('seq')} "
                                    f"lang={message.get('lang')} text={current!r}",
                                    flush=True,
                                )
                        if str(message.get("buffer_translation") or "").strip():
                            flags["translation"] = True
                    elif kind == "translation":
                        if str(message.get("translation") or "").strip():
                            flags["translation"] = True
                        print(
                            f"[translation] id={message.get('id')} "
                            f"lang={message.get('lang')} "
                            f"text={message.get('translation')!r}",
                            flush=True,
                        )
                    elif kind == "error":
                        print(f"[error] {message.get('error')}", flush=True)

            reader_task = asyncio.create_task(reader())

            # 100 ms of 16 kHz mono s16le audio per frame.
            chunk_bytes = int(16000 * 2 * 0.1)
            for offset in range(0, len(pcm), chunk_bytes):
                await ws.send(pcm[offset : offset + chunk_bytes])
                await asyncio.sleep(0.1)

            # Empty binary frame = end of stream.
            await ws.send(b"")

            deadline = asyncio.get_running_loop().time() + args.timeout
            while asyncio.get_running_loop().time() < deadline:
                if flags["text"] and flags["translation"]:
                    break
                if proc.returncode is not None and reader_task.done():
                    break
                await asyncio.sleep(0.25)

            await asyncio.sleep(0.5)
            reader_task.cancel()
            try:
                await reader_task
            except (asyncio.CancelledError, Exception):
                pass

        lines_by_id: dict[str, dict] = {}
        for message in messages:
            if message.get("type") == "captions":
                for line in message.get("lines") or []:
                    if str(line.get("text") or "").strip():
                        lines_by_id[line["id"]] = dict(line)
            elif message.get("type") == "translation":
                line_id = message.get("id")
                if line_id in lines_by_id:
                    lines_by_id[line_id]["translation"] = message.get("translation") or ""

        final_lines = [line for line in lines_by_id.values() if line.get("text")]
        translations = [
            str(line.get("translation") or "").strip()
            for line in final_lines
            if str(line.get("translation") or "").strip()
        ]

        print("", flush=True)
        print("===== SELFTEST RESULT =====", flush=True)
        print(f"device: {args.device}", flush=True)
        print(f"lines: {len(final_lines)}", flush=True)
        for index, line in enumerate(final_lines, 1):
            print(f"[{index}] 原文: {line.get('text')}", flush=True)
            print(f"[{index}] 语言: {line.get('lang')}", flush=True)
            print(f"[{index}] 译文: {line.get('translation') or '(none)'}", flush=True)

        ok = bool(final_lines)
        if not ok:
            print("[selftest] FAIL: no non-empty lines[].text", flush=True)
        if not translations:
            print("[selftest] FAIL: no non-empty translation", flush=True)
            ok = False
        if args.target.lower().startswith("zh"):
            if not any(CJK_RE.search(text) for text in translations):
                print("[selftest] FAIL: translation contains no Chinese characters", flush=True)
                ok = False

        if ok:
            print("[selftest] PASS", flush=True)
            return 0
        print("[selftest] FAIL", flush=True)
        return 1
    finally:
        stdout_task.cancel()
        try:
            await stdout_task
        except (asyncio.CancelledError, Exception):
            pass
        if proc.returncode is None:
            proc.terminate()
            try:
                await asyncio.wait_for(proc.wait(), timeout=10)
            except asyncio.TimeoutError:
                proc.kill()
                await proc.wait()


def main(argv: Optional[list[str]] = None) -> int:
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stderr.reconfigure(encoding="utf-8")
    args = parse_args(argv)
    try:
        return asyncio.run(run(args))
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
