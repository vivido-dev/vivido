"""Bounded socket producers for actual ordinary-window GPU work refusal.

Capabilities remain inherited. Private files contain only bounded commands and
public observations. No Vivid media travels through the terminal PTY.
"""
from __future__ import annotations

import json
from pathlib import Path
import sys
import time

from vivid_sdk import OverlaySession, OverlayWindowOptions
from vivid_sdk.overlay import (Brush, Canvas, KeyEvent, Path as VectorPath, Rect,
                               SubmissionOutcomeEvent, TextEvent)


def canvas(count, color):
    result = Canvas()
    bounds = Rect(0, 0, 2048, 2048)
    for _ in range(count):
        result.fill(VectorPath.rectangle(bounds), Brush.solid(color))
    return result.hit(7, VectorPath.rectangle(bounds))


def main(directory):
    sessions = [OverlaySession.from_env(), OverlaySession.from_env()]
    peer = sessions[1].create_window(OverlayWindowOptions(Rect(0, 0, 56, 56)))
    viewport = peer.viewport()
    assert viewport.width > 1300 and viewport.height > 750, viewport
    first = sessions[0].create_window(OverlayWindowOptions(Rect(0, 0, viewport.width - 56, viewport.height)))
    peer.set_bounds(Rect(viewport.width - 56, 0, 56, 56))
    windows = [first, peer]
    receipts = [first.submit(canvas(1, 0xC02020FF)), peer.submit(canvas(1, 0x2030C0FF))]
    for receipt in receipts:
        assert receipt.wait(15) == "presented", "initial native window did not present"
    first.request_focus()
    state = {"serial": 0, "stage": "ready", "revision": [r.revision for r in receipts],
             "presented": [True, True], "closed": [False, False], "keys": [0, 0],
             "text": [[], []], "colors": [0xC02020FF, 0x2030C0FF], "controls": 0,
             "bad_outcome": None, "sample_points": [[80, 120], [viewport.width - 28, 28]],
             "viewport": [viewport.width, viewport.height,
                          viewport.scale_numerator, viewport.scale_denominator]}
    def write():
        data = json.dumps(state, ensure_ascii=False)
        assert len(data.encode()) < 16384
        temporary = directory / "state.tmp"
        temporary.write_text(data)
        temporary.replace(directory / "state.json")
    write()
    print("NATIVE_REFUSAL_READY", flush=True)
    deadline = time.monotonic() + 120
    try:
        while time.monotonic() < deadline:
            command_path = directory / "command.json"
            if command_path.exists():
                data = command_path.read_bytes()
                assert len(data) <= 1024
                command = json.loads(data)
                if command["serial"] > state["serial"]:
                    assert command["serial"] == state["serial"] + 1
                    operation = command["operation"]
                    if operation == "hostile":
                        # Black half-alpha uses a compact valid color encoding.
                        # 4095 fills plus one hit stay inside 4096 commands and
                        # the existing negotiated 256 KiB record bound.
                        receipts[0] = first.submit(canvas(4095, 0x00000080))
                        until = time.monotonic() + 5
                        while first.reconcile().accepted_revision < receipts[0].revision:
                            assert time.monotonic() < until
                            time.sleep(0.001)
                        receipts[1] = peer.submit(canvas(1, 0x20B040FF))
                        state["revision"] = [r.revision for r in receipts]
                        state["presented"] = [False, False]
                        state["colors"][1] = 0x20B040FF
                    elif operation == "focus":
                        windows[command["owner"]].request_focus()
                    elif operation == "advance":
                        receipts[1] = peer.submit(canvas(1, 0xB020B0FF))
                        state["revision"][1] = receipts[1].revision
                        state["presented"][1] = False
                        state["colors"][1] = 0xB020B0FF
                    elif operation == "finish":
                        state["serial"] = command["serial"]
                        state["stage"] = "finished"
                        write()
                        return
                    else:
                        raise AssertionError("unknown refusal command")
                    state["serial"] = command["serial"]
                    state["stage"] = operation
                    print(f"NATIVE_REFUSAL_STAGE {state['serial']}", flush=True)
            state["controls"] += 1
            status = peer.reconcile()
            state["peer_status"] = [status.accepted_revision, status.presented_revision]
            if state["revision"][0] > 1:
                outcome = receipts[0].wait(0)
                if outcome is not None:
                    assert outcome == "superseded", "unrenderable native work was acknowledged"
                    state["bad_outcome"] = outcome
                if not state["closed"][0]:
                    try:
                        first.reconcile()
                    except OSError:
                        state["closed"][0] = True
            for i, session in enumerate(sessions):
                if not state["closed"][i]:
                    state["presented"][i] = receipts[i].wait(0) == "presented"
                for _ in range(32):
                    event = session.wait_event(0)
                    if event is None:
                        break
                    assert not isinstance(event, SubmissionOutcomeEvent), "duplicate/orphan terminal outcome"
                    if isinstance(event, (TextEvent, KeyEvent)):
                        assert not state["closed"][i] and event.targets(windows[i])
                        assert event.scene_revision == receipts[i].revision
                        if isinstance(event, TextEvent):
                            assert len(event.text) <= 64 and len(state["text"][i]) < 32
                            state["text"][i].append(event.text)
                        elif event.down:
                            state["keys"][i] += 1
            write()
            time.sleep(0.01)
        raise TimeoutError("ordinary refusal fixture exceeded its deadline")
    finally:
        for session in sessions:
            session.close()


if __name__ == "__main__":
    try:
        main(Path(sys.argv[1]))
    except BaseException:
        import traceback
        (Path(sys.argv[1]) / "producer-error.txt").write_text(traceback.format_exc())
        raise
