"""Real socket originals for wide nonzero winding in an owned AppKit window."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import time

from vivid_sdk import OverlaySession, OverlayWindowOptions
from vivid_sdk.overlay import Brush, Canvas, KeyEvent, Path as Shape, Rect, SubmissionOutcomeEvent, TextEvent
from native_overlay_refusal import canvas


def winding(loops):
    assert loops in [1, 127, 128, 255, 256, 682]
    path = Shape()
    delta = 384 * 0.5522847498307936
    for _ in range(loops):
        (path.move_to(896, 512)
         .cubic_to(896, 512 + delta, 512 + delta, 896, 512, 896)
         .cubic_to(512 - delta, 896, 128, 512 + delta, 128, 512)
         .cubic_to(128, 512 - delta, 512 - delta, 128, 512, 128)
         .cubic_to(512 + delta, 128, 896, 512 - delta, 896, 512).close())
    if loops == 682:
        path.move_to(512, 512).line_to(512, 512).line_to(512, 512).close()
    # The final small fill shares an edge tile with the crowded path. It checks
    # that a later packed fast fill cannot inherit the wide path's shared state.
    return (Canvas().fill(Shape.rectangle(Rect(0, 0, 2048, 2048)), Brush.solid(0xC02020FF))
            .fill(path, Brush.solid(0x20B040FF))
            .fill(Shape.rectangle(Rect(634, 148, 2, 2)), Brush.solid(0x6040D0FF))
            .hit(7, Shape.rectangle(Rect(0, 0, 2048, 2048))))


def main(directory):
    sessions = [OverlaySession.from_env(), OverlaySession.from_env()]
    peer = sessions[1].create_window(OverlayWindowOptions(Rect(0, 0, 56, 56)))
    viewport = peer.viewport()
    assert viewport.width > 1300 and viewport.height > 750
    source = sessions[0].create_window(OverlayWindowOptions(Rect(0, 0, viewport.width - 56, viewport.height)))
    peer.set_bounds(Rect(viewport.width - 56, 0, 56, 56))
    windows = [source, peer]
    receipts = [source.submit(canvas(1, 0xC02020FF)), peer.submit(canvas(1, 0x2030C0FF))]
    for receipt in receipts:
        assert receipt.wait(15) == "presented"
    source.request_focus()
    state = {"serial": 0, "stage": "ready", "revision": [1, 1], "presented": [True, True],
             "closed": [False, False], "keys": [0, 0], "text": [[], []],
             "colors": [0xC02020FF, 0x2030C0FF], "controls": 0, "winding_loops": 0,
             "source_width": viewport.width - 56, "source_height": viewport.height,
             "sample_points": [[512, 512], [viewport.width - 28, 28]],
             "reconciled": [[1, 1], [1, 1]]}
    def write():
        data = json.dumps(state, ensure_ascii=False)
        assert len(data.encode()) < 16384
        temporary = directory / "state.tmp"
        temporary.write_text(data)
        temporary.replace(directory / "state.json")
    write()
    # Keep the terminal's own glyphs/cursor in the fixed prefix excluded by the
    # image oracle. All subsequent progress is through bounded private files.
    print("NATIVE_WINDING_READY", flush=True)
    until = time.monotonic() + 180
    try:
        while time.monotonic() < until:
            path = directory / "command.json"
            if path.exists():
                data = path.read_bytes()
                assert len(data) <= 1024
                command = json.loads(data)
                if command["serial"] > state["serial"]:
                    assert command["serial"] == state["serial"] + 1
                    operation = command["operation"]
                    if operation == "winding":
                        state["winding_loops"] = command["loops"]
                        peer_color = 0xB020B0FF if state["revision"][1] % 2 else 0x2030C0FF
                        receipts = [source.submit(winding(command["loops"])), peer.submit(canvas(1, peer_color))]
                        state["revision"] = [r.revision for r in receipts]
                        state["colors"] = [0x20B040FF, peer_color]
                    elif operation == "focus":
                        windows[command["owner"]].request_focus()
                    elif operation == "advance":
                        receipts[1] = peer.submit(canvas(1, 0xB020B0FF))
                        state["revision"][1] = receipts[1].revision
                        state["colors"][1] = 0xB020B0FF
                    elif operation == "close_first":
                        source.close()
                        state["closed"][0] = True
                    elif operation == "finish":
                        assert state["closed"][0] and receipts[1].wait(0) == "presented"
                        peer.close()
                        for session in sessions:
                            session.close()
                        state["serial"] = command["serial"]
                        state["stage"] = "finished"
                        write()
                        return
                    else:
                        raise AssertionError("unknown winding fixture command")
                    state["serial"] = command["serial"]
                    state["stage"] = operation
            for i, session in enumerate(sessions):
                if not state["closed"][i]:
                    status = windows[i].reconcile()
                    state["reconciled"][i] = [status.accepted_revision, status.presented_revision]
                    state["presented"][i] = receipts[i].wait(0) == "presented"
                for _ in range(32):
                    event = session.wait_event(0)
                    if event is None:
                        break
                    assert not isinstance(event, SubmissionOutcomeEvent), "duplicate/orphan outcome"
                    if isinstance(event, (TextEvent, KeyEvent)):
                        assert not state["closed"][i] and event.targets(windows[i])
                        assert event.scene_revision == receipts[i].revision
                        if isinstance(event, TextEvent):
                            assert len(event.text) <= 64 and len(state["text"][i]) < 32
                            state["text"][i].append(event.text)
                        elif event.down:
                            state["keys"][i] += 1
            state["controls"] += 1
            write()
            time.sleep(0.005)
        raise TimeoutError("native winding fixture exceeded its deadline")
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
