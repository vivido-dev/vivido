"""Two socket producers for the ordinary-window checked-overlay acceptance.

Run only as the child of the dedicated qualification Vivido window. Capabilities
are inherited in memory/environment and never copied into reports or arguments.
The private directory carries bounded commands and public acceptance observations.
"""
from __future__ import annotations

import json
from pathlib import Path
import sys
import time

from vivid_sdk import OverlaySession, OverlayWindowOptions
from vivid_sdk.overlay import Brush, Canvas, KeyEvent, Path as VectorPath, Point, Rect, TextEvent


def canvas(color, label):
    bounds = Rect(0, 0, 180, 120)
    return (Canvas().fill(VectorPath.rectangle(bounds), Brush.solid(color))
            .text(label, Point(12, 20), 18, 0xFFFFFFFF)
            .hit(7, VectorPath.rectangle(bounds)))


def main(directory):
    sessions = [OverlaySession.from_env(), OverlaySession.from_env()]
    windows = [sessions[i].create_window(OverlayWindowOptions(Rect(40 + i * 240, 40, 180, 120)))
               for i in range(2)]
    receipts = [windows[i].submit(canvas(color, label)) for i, (color, label) in
                enumerate([(0xC02020FF, "Owner one"), (0x2030C0FF, "Same-ID peer")])]
    for receipt in receipts:
        assert receipt.wait(10) == "presented", "initial real window presentation failed"
    state = {"serial": 0, "stage": "ready", "presented": [True, True],
             "revision": [r.revision for r in receipts], "text": [[], []],
             "keys": [0, 0], "closed": [False, False],
             "updates": 0, "colors": [0xC02020FF, 0x2030C0FF]}
    def write():
        encoded = json.dumps(state, ensure_ascii=False)
        assert len(encoded.encode()) < 16384
        temporary = directory / "state.tmp"
        temporary.write_text(encoded)
        temporary.replace(directory / "state.json")
    def focus(index):
        windows[index].request_focus()
        windows[index].set_editor_geometry(receipts[index].revision, Rect(12, 60, 2, 22))
    focus(0)
    write()
    print("NATIVE_OVERLAY_READY", flush=True)
    deadline = time.monotonic() + 180
    try:
        while time.monotonic() < deadline:
            changed = False
            command_path = directory / "command.json"
            if command_path.exists():
                data = command_path.read_bytes()
                assert len(data) <= 1024
                command = json.loads(data)
                if command["serial"] > state["serial"]:
                    assert command["serial"] == state["serial"] + 1
                    operation = command["operation"]
                    if operation == "advance":
                        state["updates"] += 1
                        colors = (0x20B040FF, 0xB08020FF) if state["updates"] % 2 else (0xC02020FF, 0x2030C0FF)
                        state["colors"] = list(colors)
                        for i in range(2):
                            if not state["closed"][i]:
                                receipts[i] = windows[i].submit(canvas(colors[i], f"Owner {i + 1}: {command['serial']}"))
                                state["presented"][i] = False
                                state["revision"][i] = receipts[i].revision
                    elif operation == "reconcile":
                        statuses = [window.reconcile() for window in windows]
                        state["reconciled"] = [[s.accepted_revision, s.presented_revision] for s in statuses]
                    elif operation == "focus":
                        focus(command["owner"])
                    elif operation == "close_first":
                        windows[0].close()
                        state["closed"][0] = True
                    elif operation == "finish":
                        state["stage"] = "finished"
                        state["serial"] = command["serial"]
                        write()
                        break
                    else:
                        raise AssertionError("unknown bounded acceptance command")
                    state["stage"] = operation
                    state["serial"] = command["serial"]
                    changed = True
                    print(f"NATIVE_OVERLAY_STAGE {state['serial']}", flush=True)
            for i, session in enumerate(sessions):
                if not state["closed"][i] and not state["presented"][i]:
                    if receipts[i].wait(0) == "presented":
                        state["presented"][i] = True
                        changed = True
                event = session.wait_event(0.01)
                if event is not None and not state["closed"][i] and event.targets(windows[i]):
                    if isinstance(event, TextEvent):
                        assert len(state["text"][i]) < 32 and len(event.text) <= 64
                        assert event.scene_revision == receipts[i].revision, "text used a stale scene"
                        state["text"][i].append(event.text)
                        changed = True
                    elif isinstance(event, KeyEvent) and event.down:
                        assert event.scene_revision == receipts[i].revision, "key used a stale scene"
                        state["keys"][i] += 1
                        changed = True
            if changed:
                write()
        else:
            raise TimeoutError("ordinary overlay acceptance exceeded its finite bound")
    finally:
        for i, window in enumerate(windows):
            if not state["closed"][i]:
                window.close()
        for session in sessions:
            session.close()
    print("NATIVE_OVERLAY_DONE", flush=True)


if __name__ == "__main__":
    try:
        main(Path(sys.argv[1]))
    except Exception:
        import traceback
        (Path(sys.argv[1]) / "producer-error.txt").write_text(traceback.format_exc())
        raise
