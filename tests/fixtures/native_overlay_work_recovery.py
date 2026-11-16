"""Original hostile receipts and healthy peers across native recovery during diagnosis."""
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
    assert viewport.width > 1300 and viewport.height > 750
    first = sessions[0].create_window(OverlayWindowOptions(Rect(0, 0, viewport.width - 56, viewport.height)))
    peer.set_bounds(Rect(viewport.width - 56, 0, 56, 56))
    windows = [first, peer]
    receipts = [first.submit(canvas(1, 0xC02020FF)), peer.submit(canvas(1, 0x2030C0FF))]
    for receipt in receipts:
        assert receipt.wait(15) == "presented"
    first.request_focus()
    bad = None
    state = {"serial": 0, "stage": "ready", "revision": [1, 1],
             "presented": [True, True], "closed": [False, False], "keys": [0, 0],
             "text": [[], []], "colors": [0xC02020FF, 0x2030C0FF], "controls": 0,
             "bad_outcome": None, "retained_hostile": False, "allow_hostile_presented": False,
             "sample_points": [[80, 120], [viewport.width - 28, 28]],
             "viewport": [viewport.width, viewport.height, viewport.scale_numerator, viewport.scale_denominator]}
    def write():
        text = json.dumps(state, ensure_ascii=False)
        assert len(text.encode()) < 16384
        temporary = directory / "state.tmp"
        temporary.write_text(text)
        temporary.replace(directory / "state.json")
    write()
    print("NATIVE_WORK_RECOVERY_READY", flush=True)
    deadline = time.monotonic() + 120
    try:
        while time.monotonic() < deadline:
            path = directory / "command.json"
            if path.exists():
                data = path.read_bytes()
                assert len(data) <= 1024
                command = json.loads(data)
                if command["serial"] > state["serial"]:
                    assert command["serial"] == state["serial"] + 1
                    operation = command["operation"]
                    if operation == "hostile":
                        state["allow_hostile_presented"] = command.get("allow_presented", False)
                        assert isinstance(state["allow_hostile_presented"], bool)
                        bad = first.submit(canvas(4095, 0x00000080))
                        receipts[0] = bad
                        until = time.monotonic() + 5
                        while first.reconcile().accepted_revision < bad.revision:
                            assert time.monotonic() < until
                            time.sleep(0.001)
                        state["revision"][0] = bad.revision
                        state["presented"][0] = False
                    elif operation in ["replace", "retain"]:
                        assert bad is not None and not state["closed"][0]
                        viewport = peer.viewport()
                        assert viewport.width > 100 and viewport.height > 120
                        peer.set_bounds(Rect(viewport.width - 56, 0, 56, 56))
                        if operation == "replace":
                            first.set_bounds(Rect(0, 0, viewport.width - 56, viewport.height))
                            receipts[0] = first.submit(canvas(1, 0x20C0C0FF))
                            state["colors"][0] = 0x20C0C0FF
                        else:
                            # Keep the source bytes, bounds and original receipt.
                            # Retargeting the native renderer must invalidate old proof.
                            state["retained_hostile"] = True
                            assert receipts[0] is bad
                            if state["allow_hostile_presented"]:
                                # The source keeps its original large bounds;
                                # raise this owner's peer above that overlap.
                                peer.raise_window()
                                state["colors"][0] = 0x000000FF
                        receipts[1] = peer.submit(canvas(1, 0x20B040FF))
                        state["revision"] = [r.revision for r in receipts]
                        assert state["revision"] == ([3, 2] if operation == "replace" else [2, 2])
                        state["presented"] = [False, False]
                        state["colors"][1] = 0x20B040FF
                        state["sample_points"] = [[80, 120], [viewport.width - 28, 28]]
                        state["viewport"] = [viewport.width, viewport.height,
                                             viewport.scale_numerator, viewport.scale_denominator]
                    elif operation == "focus":
                        windows[command["owner"]].request_focus()
                    elif operation == "advance":
                        receipts[1] = peer.submit(canvas(1, 0xB020B0FF))
                        state["revision"][1] = receipts[1].revision
                        state["presented"][1] = False
                        state["colors"][1] = 0xB020B0FF
                    elif operation == "finish":
                        assert bad.wait(0) == ("presented" if state["allow_hostile_presented"] else "superseded")
                        assert receipts[1].wait(0) == "presented"
                        if state["retained_hostile"] and not state["allow_hostile_presented"]:
                            assert state["closed"][0]
                        else:
                            assert receipts[0].wait(0) == "presented" and not state["closed"][0]
                        state["serial"] = command["serial"]
                        state["stage"] = "finished"
                        write()
                        return
                    else:
                        raise AssertionError("unknown native recovery command")
                    state["serial"] = command["serial"]
                    state["stage"] = operation
                    print(f"NATIVE_WORK_RECOVERY_STAGE {state['serial']}", flush=True)
            statuses = []
            for i, window in enumerate(windows):
                if state["closed"][i]:
                    statuses.append(None)
                    continue
                try:
                    statuses.append(window.reconcile())
                except OSError:
                    assert i == 0 and state["retained_hostile"] and not state["allow_hostile_presented"], "unrelated or healthy owner closed"
                    state["closed"][i] = True
                    statuses.append(None)
            state["controls"] += 1
            state["reconciled"] = [[s.accepted_revision, s.presented_revision] if s else None for s in statuses]
            state["presented"] = [r.wait(0) == "presented" for r in receipts]
            if bad is not None:
                outcome = bad.wait(0)
                assert state["allow_hostile_presented"] or outcome != "presented", "old hostile work was acknowledged"
                state["bad_outcome"] = outcome
            for i, session in enumerate(sessions):
                for _ in range(32):
                    event = session.wait_event(0)
                    if event is None:
                        break
                    assert not isinstance(event, SubmissionOutcomeEvent), "duplicate/orphan original outcome"
                    if isinstance(event, (TextEvent, KeyEvent)):
                        assert not state["closed"][i] and event.targets(windows[i])
                        assert event.scene_revision == receipts[i].revision
                        if isinstance(event, TextEvent):
                            assert len(event.text) <= 64 and len(state["text"][i]) < 32
                            state["text"][i].append(event.text)
                        elif event.down:
                            state["keys"][i] += 1
            write()
            time.sleep(0.005)
        raise TimeoutError("native work recovery fixture exceeded deadline")
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
