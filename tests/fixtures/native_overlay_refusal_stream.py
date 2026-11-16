"""Eight authenticated peers through ordinary native checked presentations.

Only bounded public observations/commands use private files or the PTY. All
Vivid media and exact scene input use their original authenticated sockets.
"""
from __future__ import annotations

import json
from pathlib import Path
import sys
import time

from vivid_sdk import OverlaySession, OverlayWindowOptions
from vivid_sdk.overlay import Rect, SubmissionOutcomeEvent, TextEvent, KeyEvent
from native_overlay_refusal import canvas


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
        assert receipt.wait(15) == "presented"
    first.request_focus()
    state = {"serial": 0, "stage": "ready", "revision": [r.revision for r in receipts],
             "presented": [True, True], "closed": [False, False], "keys": [0, 0],
             "text": [[], []], "colors": [0xC02020FF, 0x2030C0FF], "controls": 0,
             "bad_outcome": None, "sample_points": [[80, 120], [viewport.width - 28, 28]],
             "viewport": [viewport.width, viewport.height, viewport.scale_numerator, viewport.scale_denominator],
             "stream": []}
    originals = []
    def write():
        data = json.dumps(state, ensure_ascii=False)
        assert len(data.encode()) < 16384
        temporary = directory / "state.tmp"
        temporary.write_text(data)
        temporary.replace(directory / "state.json")
    def drain():
        for i, session in enumerate(sessions):
            for _ in range(32):
                event = session.wait_event(0)
                if event is None:
                    break
                assert not isinstance(event, SubmissionOutcomeEvent), "duplicate/orphan original outcome"
                if isinstance(event, (TextEvent, KeyEvent)):
                    assert not state["closed"][i] and event.targets(windows[i])
                    assert event.scene_revision == receipts[i].revision, (event.scene_revision, receipts[i].revision)
                    if isinstance(event, TextEvent):
                        assert len(event.text) <= 64 and len(state["text"][i]) < 32
                        state["text"][i].append(event.text)
                    elif event.down:
                        state["keys"][i] += 1
    def peer_status():
        status = peer.reconcile()
        state["controls"] += 1
        state["peer_status"] = [status.accepted_revision, status.presented_revision]
        return status
    write()
    print("NATIVE_STREAM_READY", flush=True)
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
                    if operation == "stream_hostile":
                        peer.request_focus()
                        # Admit the new focus/stack before the hostile candidate,
                        # without a test-only focus or drawing-admission helper.
                        receipts[1] = peer.submit(canvas(1, 0x2030C0FF))
                        assert receipts[1].revision == 2 and receipts[1].wait(15) == "presented"
                        receipts[0] = first.submit(canvas(4095, 0x00000080))
                        until = time.monotonic() + 5
                        while first.reconcile().accepted_revision < receipts[0].revision:
                            assert time.monotonic() < until
                            time.sleep(0.001)
                        state["revision"] = [r.revision for r in receipts]
                        state["presented"] = [False, True]
                    elif operation == "stream":
                        assert state["stage"] == "stream_hostile"
                        for revision in range(3, 11):
                            assert receipts[0].wait(0) is None, "bad scene refused before peer burst"
                            color = (revision * 16 << 24) | (192 << 16) | (80 << 8) | 255
                            receipt = peer.submit(canvas(1, color))
                            assert receipt.revision == revision
                            receipts[1] = receipt
                            originals.append(receipt)
                            initial_pending = receipt.wait(0) is None
                            state["revision"][1] = revision
                            state["presented"][1] = False
                            state["colors"][1] = color
                            expected = f"window stream Ω {revision}"
                            until = time.monotonic() + 5
                            while receipt.wait(0) != "presented" or expected not in state["text"][1]:
                                assert receipts[0].wait(0) is None, "private refusal overtook burst"
                                drain()
                                assert time.monotonic() < until, ("native peer receipt/input stalled", revision)
                                time.sleep(0.001)
                            assert receipts[0].wait(0) is None
                            status = peer_status()
                            assert status.accepted_revision == status.presented_revision == revision
                            state["presented"][1] = True
                            state["stream"].append({"revision": revision, "initial_pending": initial_pending,
                                                    "outcome": receipt.wait(0), "bad_outcome": None,
                                                    "scene_text": expected})
                            write()
                    elif operation == "focus":
                        windows[command["owner"]].request_focus()
                    elif operation == "advance":
                        receipts[1] = peer.submit(canvas(1, 0xB020B0FF))
                        state["revision"][1] = receipts[1].revision
                        state["presented"][1] = False
                        state["colors"][1] = 0xB020B0FF
                    elif operation == "finish":
                        assert len(originals) == 8 and all(r.wait(0) == "presented" for r in originals)
                        state["serial"] = command["serial"]
                        state["stage"] = "finished"
                        write()
                        return
                    else:
                        raise AssertionError("unknown native stream command")
                    state["serial"] = command["serial"]
                    state["stage"] = operation
                    print(f"NATIVE_STREAM_STAGE {state['serial']}", flush=True)
            peer_status()
            if state["revision"][0] > 1:
                outcome = receipts[0].wait(0)
                if outcome is not None:
                    assert outcome == "superseded", "hostile work was Presented"
                    state["bad_outcome"] = outcome
                if not state["closed"][0]:
                    try:
                        first.reconcile()
                    except OSError:
                        state["closed"][0] = True
            for i in range(2):
                if not state["closed"][i]:
                    state["presented"][i] = receipts[i].wait(0) == "presented"
            drain()
            write()
            time.sleep(0.005)
        raise TimeoutError("native stream fixture exceeded deadline")
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
