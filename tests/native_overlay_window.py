"""Finite macOS acceptance of the ordinary Vivido checked-overlay window.

Build Vivido and the SDK Python binding, unlock the Mac, then run this file.
Only its own process, window, private IPC endpoint and fixture files are touched.
UI-routed input is synthetic; physical keyboard/IME/scanout remain unqualified.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


SCREEN_CAPTURE_PERMISSION = (
    "Native window acceptance requires macOS Screen Recording permission for the app "
    "running cargo test (for example Vivido). Enable that app in System Settings > "
    "Privacy & Security > Screen & System Audio Recording, then quit and reopen it."
)


def validate_preflight(preflight):
    if preflight["locked"]:
        raise RuntimeError("ordinary window acceptance requires an unlocked macOS session")
    if not preflight["screen_capture_allowed"]:
        raise RuntimeError(SCREEN_CAPTURE_PERMISSION)


def run(binary, python, output, work_refusal=False, stream_refusal=False, work_recovery=None, wide_winding=False):
    retained_hostile = bool(work_recovery and work_recovery.startswith("retain-"))
    lease_resume = bool(work_recovery and work_recovery.startswith("lease-"))
    recovery_action = work_recovery.removeprefix("retain-") if work_recovery else None
    if lease_resume:
        recovery_action = "hide" if work_recovery == "lease-hide" else "resume"
    allow_retained_presented = recovery_action == "downsize"
    refuse_retained = retained_hostile and not allow_retained_presented
    work_refusal = work_refusal or stream_refusal or work_recovery is not None or wide_winding
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    config = output / "config.toml"
    columns, rows = (150, 42) if work_refusal else (100, 28)
    config.write_text('[window]\n' + f'dimensions = {{ columns = {columns}, lines = {rows} }}\n'
                      '[font]\nsize = 16\n[general]\nlive_config_reload = false\n')
    if work_refusal:
        with config.open("a") as stream:
            stream.write('[colors.primary]\nbackground = "#182028"\n')
    producer = Path(__file__).parent / "fixtures" / (
        "native_overlay_winding.py" if wide_winding else
        "native_overlay_work_recovery.py" if work_recovery else
        "native_overlay_refusal_stream.py" if stream_refusal else
        "native_overlay_refusal.py" if work_refusal else "native_overlay_window.py")
    producer_command = [str(python), str(producer), str(output)]
    if lease_resume:
        helper = binary.parent / "examples/native_overlay_lease_producer"
        assert helper.is_file(), "build --features test-util --example native_overlay_lease_producer"
        producer_command = [str(helper), str(output)]
    command = [str(binary), "--config-file", str(config), "--socket", str(output / "ipc.sock"),
               "--automation-name", "native-overlay-qualification", "--title",
               "Vivido checked overlay qualification", "-e", *producer_command]
    actions = []
    report = {"exit_code": 1, "command": command, "actions": actions, "work_refusal": work_refusal, "stream_refusal": stream_refusal, "work_recovery": work_recovery, "wide_winding": wide_winding}
    preflight = subprocess.run([
        "swift", str(Path(__file__).parent / "fixtures/native_overlay_pixels.swift"), "--preflight",
    ], capture_output=True, text=True, timeout=30)
    assert preflight.returncode == 0, preflight.stderr
    report["preflight"] = json.loads(preflight.stdout)
    (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Native window evidence: {output}", flush=True)
    validate_preflight(report["preflight"])
    environment = dict(os.environ)
    environment.pop("VIVIDO_OVERLAY_STREAM_CHECK", None)
    if stream_refusal:
        environment["VIVIDO_OVERLAY_STREAM_CHECK"] = "1"
    with (output / "window.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True, env=environment)
    report["pid"] = process.pid
    def save():
        (output / "result.json").write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    def msg(*arguments):
        started = time.monotonic()
        result = subprocess.run([str(binary), "msg", "--socket", str(output / "ipc.sock"),
                                 *map(str, arguments)], capture_output=True, text=True, timeout=15)
        if result.returncode:
            actions.append({"arguments": list(arguments), "exit_code": result.returncode,
                            "stdout": result.stdout, "stderr": result.stderr})
            if arguments[0] == "focus":
                observed = subprocess.run([
                    str(binary), "msg", "--socket", str(output / "ipc.sock"),
                    "inspect", "--window-id", str(window),
                ], capture_output=True, text=True, timeout=10)
                report["focus_failure_inspect"] = observed.stdout
            save()
            raise AssertionError((arguments, result.stderr))
        try:
            data = json.loads(result.stdout)
        except json.JSONDecodeError:
            data = result.stdout
        actions.append({"arguments": list(arguments), "seconds": time.monotonic() - started,
                        "result": data})
        save()
        return data
    def state():
        return json.loads((output / "state.json").read_text())
    def wait(predicate, label, timeout=15, allow_exit=False):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            if (output / "state.json").exists():
                value = state()
                if predicate(value):
                    assert allow_exit or process.poll() is None, "qualification window exited; see window.log"
                    return value
            assert process.poll() is None, "qualification window exited; see window.log"
            time.sleep(0.02)
        raise AssertionError((label, state() if (output / "state.json").exists() else "no state"))
    def request(operation, **data):
        # AppKit image helpers can take native focus while inspecting a capture.
        # Focus the pane before asking its producer to acquire overlay input.
        if operation == "focus":
            focus_native()
        serial = state()["serial"] + 1
        temporary = output / "command.tmp"
        temporary.write_text(json.dumps({"serial": serial, "operation": operation, **data}))
        temporary.replace(output / "command.json")
        return wait(lambda s: s["serial"] == serial, operation, allow_exit=operation == "finish")
    def inspect():
        return msg("inspect", "--window-id", window)
    def focus_native():
        # Deminiaturization may already have delivered a real focus event. The
        # IPC focus operation intentionally awaits a newer confirmation edge.
        if not inspect()["window"]["focused"]:
            msg("focus", "--window-id", window)
        assert inspect()["window"]["focused"]
    def capture(name):
        result = msg("screenshot", "--window-id", window, "--json")
        shutil.copy2(result["path"], output / (name + ".png"))
        result["producer_state"] = state()
        points = result["producer_state"].get("sample_points", [[80, 120], [320, 120]])
        coordinates = [str(round(value * result["scale_factor"])) for point in points for value in point]
        samples = subprocess.run([
            "swift", str(Path(__file__).parent / "fixtures/native_overlay_pixels.swift"),
            str(output / (name + ".png")), *coordinates,
        ], capture_output=True, text=True, timeout=30)
        assert samples.returncode == 0, samples.stderr
        result["sampled_rgb"] = json.loads(samples.stdout)
        for i, rgb in enumerate(result["sampled_rgb"]):
            color = result["producer_state"]["colors"][i]
            expected = [(color >> shift) & 255 for shift in [24, 16, 8]]
            if result["producer_state"]["closed"][i]:
                if work_refusal:
                    assert all(abs(a-b) <= 1 for a, b in zip(rgb, [24, 32, 40])), rgb
                else:
                    assert rgb not in [[192, 32, 32], [32, 176, 64]], rgb
            else:
                assert all(abs(a - b) <= 1 for a, b in zip(rgb, expected)), (name, rgb, expected)
        if name in ["initial", "survivor", "recovered"] or name.startswith("winding-"):
            owned = subprocess.run([
                "swift", str(Path(__file__).parent / "fixtures/native_overlay_pixels.swift"),
                "--window", str(process.pid),
            ], capture_output=True, text=True, timeout=30)
            assert owned.returncode == 0, owned.stderr
            windows = json.loads(owned.stdout)
            assert len(windows) == 1 and windows[0]["onscreen"], windows
            result["native_window"] = windows[0]
            captured = subprocess.run([
                "/usr/sbin/screencapture", "-x", "-o", "-l", str(windows[0]["id"]),
                str(output / (name + "-window.png")),
            ], capture_output=True, text=True, timeout=15)
            result["native_capture"] = {"exit_code": captured.returncode,
                                        "stderr": captured.stderr[:4096]}
            assert captured.returncode == 0, (
                f"OS capture of owned window {windows[0]['id']} failed: {captured.stderr.strip()}. "
                f"{SCREEN_CAPTURE_PERMISSION} Evidence: {output}"
            )
            assert (output / (name + "-window.png")).stat().st_size > 0
        (output / (name + "-capture.json")).write_text(json.dumps(result, indent=2) + "\n")
        return result
    def hidden_hold(name):
        before = inspect()
        assert before["window"]["occluded"] or not before["window"]["visible"], before
        request("advance")
        start = time.monotonic()
        while time.monotonic() - start < 6:
            assert state()["presented"] == [False, False], state()
            time.sleep(0.1)
        reconciled = request("reconcile")
        assert reconciled["reconciled"] == [[r, r - 1] for r in reconciled["revision"]], reconciled
        after = inspect()
        assert after["window"]["sequences"]["output"] > before["window"]["sequences"]["output"]
        assert after["window"]["sequences"]["frame"] == before["window"]["sequences"]["frame"]
        report[name] = {"seconds": time.monotonic() - start, "before": before, "after": after,
                        "producer_state": state()}
        save()
    def hostile_work():
        before = inspect()
        request("hostile")
        wait(lambda s: s["bad_outcome"] == "superseded" and s["closed"][0]
             and s["presented"][1], "real native refusal and surviving original receipt", 30)
        after = inspect()
        assert after["vivid_overlay"]["windows"] == 1
        assert after["vivid_overlay"]["input_lanes"] == 2
        assert after["window"]["creation_index"] == before["window"]["creation_index"]
        passes = after["render_optimization"]["overlay_render_passes"] - before["render_optimization"]["overlay_render_passes"]
        assert passes >= 128, ("actual mixed and isolated checked work bounds", passes)
        sessions = msg("vivid", "sessions", "--window-id", window)
        assert {s["session_id"] for s in sessions["sessions"]} == {s["session_id"] for s in report["sessions"]["sessions"]}
        report["refusal"] = {"before": before, "after": after, "passes": passes,
                             "sessions_after": sessions, "producer": state()}
        capture("survivor")
        request("focus", owner=1)
        msg("key", "B", "--route", "ui", "--window-id", window)
        msg("paste", "survivor Ω", "--route", "ui", "--window-id", window)
        wait(lambda s: s["keys"][1] > 0 and "survivor Ω" in "".join(s["text"][1]), "surviving peer input")
        assert "survivor Ω" not in "".join(state()["text"][0])
        request("advance")
        wait(lambda s: s["presented"][1], "fresh peer output on the same worker")
        capture("fresh")
    def recovery_work():
        def replace_or_retain():
            if lease_resume:
                resumed = request("resume")
                assert resumed["resumed"] and resumed["old_receipt_retired"] and resumed["focus_before_fresh_denied"]
                assert resumed["reconciled"][0][1] == resumed["reconciled"][1][1] == 1
                report["recovery"]["resume"] = {"producer": resumed, "tracks": msg("vivid", "tracks", "--window-id", window)}
            request("retain" if retained_hostile else "replace")
        before = inspect()
        assert "overlay_stream_check" in before and not before["overlay_stream_check"]["enabled"], "build test-util metadata without activating the pixel/input observer"
        request("hostile", allow_presented=allow_retained_presented)
        until = time.monotonic() + 5
        while True:
            active = inspect()
            live_passes = active["overlay_stream_check"]["probe_passes"]
            if live_passes is not None and live_passes <= 32:
                break
            assert state()["bad_outcome"] is None and time.monotonic() < until, "early private work was not witnessed"
        assert state()["bad_outcome"] is None, "original receipt resolved before native retargeting"
        report["recovery"] = {"mode": work_recovery, "before": before, "private_active": active}
        save()
        if recovery_action == "resume":
            replace_or_retain()
        elif recovery_action in ["resize", "downsize"]:
            # The smaller target can legitimately complete the old original.
            # Keep strict Superseded assertions on the larger refusal target;
            # retain-downsize separately verifies successful original admission.
            width, height = (2296, 1312) if recovery_action == "downsize" else (2816, 1536)
            msg("resize", "--width", width, "--height", height, "--window-id", window)
            until = time.monotonic() + 5
            while True:
                resized = inspect()
                if resized["window"]["pixels"] == {"width": width, "height": height} and resized["overlay_stream_check"]["target_size"] == [width, height]:
                    break
                assert time.monotonic() < until, "native target did not reach exact even dimensions"
            report["recovery"]["resized"] = resized
            replace_or_retain()
        else:
            if recovery_action == "hide":
                msg("set-visible", "--visible", "false", "--window-id", window)
            else:
                assert recovery_action == "minimize"
                msg("key", "m", "--mods", "Super", "--route", "ui", "--window-id", window)
            until = time.monotonic() + 5
            while True:
                hidden = inspect()
                if hidden["window"]["occluded"] or not hidden["window"]["visible"]:
                    break
                assert time.monotonic() < until, "native window did not become hidden"
            replace_or_retain()
            start = time.monotonic()
            controls = state()["controls"]
            while time.monotonic() - start < 6:
                current = state()
                assert current["presented"] == [False, False], "hidden original receipts were acknowledged"
                assert current["bad_outcome"] != "presented" and current["closed"] == [False, False]
                time.sleep(0.1)
            retired = inspect()
            assert retired["overlay_stream_check"]["hidden_released"], "normal five-second GPU release did not run"
            assert retired["window"]["sequences"]["frame"] == hidden["window"]["sequences"]["frame"]
            assert retired["window"]["sequences"]["output"] > hidden["window"]["sequences"]["output"]
            assert state()["controls"] > controls
            assert state()["reconciled"] == [[2 if retained_hostile else 3, 1], [2, 1]], state()
            report["recovery"]["hidden"] = {"before": hidden, "after": retired,
                                             "seconds": time.monotonic() - start, "producer": state()}
            save()
            msg("set-visible", "--visible", "true", "--window-id", window)
            focus_native()
        expected_presented = [not refuse_retained, True]
        expected_closed = [refuse_retained, False]
        expected_reconciled = [None if refuse_retained else [2, 2] if retained_hostile else [3, 3], [2, 2]]
        expected_outcome = "retired" if lease_resume else "presented" if allow_retained_presented else "superseded"
        recovered = wait(lambda s: s["presented"] == expected_presented and s["bad_outcome"] == expected_outcome
                         and s["closed"] == expected_closed and s["reconciled"] == expected_reconciled,
                         "checked original outcomes and peer admission after native recovery", 30)
        after = inspect()
        assert after["vivid_overlay"]["windows"] == (1 if refuse_retained else 2)
        assert after["vivid_overlay"]["input_lanes"] == 2
        assert after["window"]["creation_index"] == before["window"]["creation_index"]
        sessions = msg("vivid", "sessions", "--window-id", window)
        identity = lambda s: (s["session_id"], s["presenter_instance_id"])
        assert {identity(s) for s in sessions["sessions"]} == {identity(s) for s in report["sessions"]["sessions"]}
        tracks = msg("vivid", "tracks", "--window-id", window)
        identity = lambda t: (t["session_id"], t["context_id"], t["surface_id"], t["track_id"], t["channel_generation"], t["media_epoch"])
        initial_tracks = {identity(t) for t in report["tracks"]["tracks"]}
        if lease_resume:
            source = recovered["source_owner"]
            initial_tracks = {(s, c, w, t, g + (1 if s == source else 0), e)
                              for s, c, w, t, g, e in initial_tracks}
        live_tracks = {identity(t) for t in tracks["tracks"]}
        # Refusal preserves authenticated tracks. Lease resume preserves logical
        # identity while advancing only that owner's media channel.
        assert live_tracks == initial_tracks
        if retained_hostile:
            reset = report["recovery"].get("resized") or report["recovery"]["hidden"]["after"]
            report["recovery"]["passes_after_retarget"] = (
                after["render_optimization"]["overlay_render_passes"] - reset["render_optimization"]["overlay_render_passes"])
            if refuse_retained:
                assert report["recovery"]["passes_after_retarget"] >= 128, "fresh mixed and isolated work bounds were not observed"
        report["recovery"].update({"after": after, "producer": recovered, "sessions_after": sessions, "tracks_after": tracks})
        capture("recovered")
        if lease_resume:
            request("stale_cancel")
        if not refuse_retained:
            request("focus", owner=0)
            msg("paste", "recovered Ω", "--route", "ui", "--window-id", window)
            wait(lambda s: "recovered Ω" in "".join(s["text"][0]), "fresh source input")
        request("focus", owner=1)
        msg("key", "B", "--route", "ui", "--window-id", window)
        msg("paste", "peer recovered Ω", "--route", "ui", "--window-id", window)
        wait(lambda s: s["keys"][1] > 0 and "peer recovered Ω" in "".join(s["text"][1]), "unrelated peer input")
        assert "peer recovered Ω" not in "".join(state()["text"][0])
        request("advance")
        wait(lambda s: s["presented"] == expected_presented, "later peer output after recovery")
        capture("fresh")
        if lease_resume:
            assert request("stale_cancel")["stale_cancel_calls"] == 2
    def winding_work():
        rows = []
        report["winding"] = {"before": inspect(), "rows": rows}
        assert not report["winding"]["before"].get("overlay_stream_check", {}).get("enabled", False)
        for loops in [1, 127, 128, 255, 256, 682]:
            submitted = request("winding", loops=loops)
            revision = submitted["revision"][0]
            admitted = wait(lambda s: s["presented"] == [True, True]
                            and s["reconciled"] == [[revision, revision], [revision, revision]],
                            "original wide-winding native receipt")
            name = f"winding-{loops}"
            captured = capture(name)
            peer_rgb = [(admitted["colors"][1] >> shift) & 255 for shift in [24, 16, 8]]
            result = subprocess.run([
                "swift", str(Path(__file__).parent / "fixtures/native_overlay_winding.swift"),
                str(output / (name + ".png")), str(admitted["source_width"]),
                str(admitted["source_height"]), str(captured["scale_factor"]), *map(str, peer_rgb),
            ], capture_output=True, text=True, timeout=30)
            observation = json.loads(result.stdout) if result.stdout else {"error": result.stderr}
            row = {"loops": loops, "revision": revision, "producer": admitted,
                   "capture": name + "-capture.json", "oracle": observation, "exit_code": result.returncode}
            rows.append(row)
            save()
            assert result.returncode == 0, ("native winding coverage", row)
            assert observation["mismatched_pixels"] == 0
        request("focus", owner=0)
        msg("paste", "wide source Ω", "--route", "ui", "--window-id", window)
        wait(lambda s: "wide source Ω" in "".join(s["text"][0]), "wide source input")
        request("focus", owner=1)
        msg("key", "B", "--route", "ui", "--window-id", window)
        msg("paste", "wide peer Ω", "--route", "ui", "--window-id", window)
        wait(lambda s: s["keys"][1] > 0 and "wide peer Ω" in "".join(s["text"][1]), "wide same-ID peer input")
        assert "wide peer Ω" not in "".join(state()["text"][0])
        request("close_first")
        request("advance")
        wait(lambda s: s["presented"][1] and s["reconciled"][1] == [8, 8], "wide peer survives source close")
        after = inspect()
        assert after["vivid_overlay"]["windows"] == 1 and after["vivid_overlay"]["input_lanes"] == 2
        assert after["window"]["creation_index"] == report["winding"]["before"]["window"]["creation_index"]
        report["winding"]["after"] = after
        report["winding"]["sessions_after"] = msg("vivid", "sessions", "--window-id", window)
        capture("survivor")
    def streaming_work():
        before = inspect()
        assert before.get("overlay_stream_check", {}).get("enabled"), "build the native observer with --features test-util"
        request("stream_hostile")
        until = time.monotonic() + 5
        while not inspect()["overlay_stream_check"]["diagnosing"]:
            assert state()["bad_outcome"] is None and time.monotonic() < until, "actual private diagnosis was not observed"
        request("stream")
        wait(lambda s: s["closed"][0] and s["bad_outcome"] == "superseded" and s["presented"][1],
             "post-burst isolated native refusal", 30)
        until = time.monotonic() + 5
        while True:
            after = inspect()
            observation = after["overlay_stream_check"]
            assert observation["error"] is None, observation
            if observation["pending"] == 0 and len(observation["rows"]) == 8:
                break
            assert time.monotonic() < until, ("bounded native readbacks stalled", observation)
        rows = sorted(observation["rows"], key=lambda r: r["revision"])
        assert [r["revision"] for r in rows] == list(range(3, 11)), rows
        assert observation["worker_admitted"] == 8 and observation["shared_attempts"] == 72, observation
        assert observation["max_rows"] == 8 and observation["staging_bytes_per_row"] == 512
        assert all(r["during_diagnosis"] and r["input_requested"] for r in rows)
        assert all(r["pixels"] == [[192, 32, 32, 255], [r["revision"] * 16, 192, 80, 255]] for r in rows), rows
        assert all(a["native_frame"] < b["native_frame"] and a["overlay_generation"] < b["overlay_generation"]
                   for a, b in zip(rows, rows[1:])), rows
        producer = state()
        assert [r["revision"] for r in producer["stream"]] == list(range(3, 11))
        assert all(r["outcome"] == "presented" and r["bad_outcome"] is None for r in producer["stream"])
        assert all(r["scene_text"] == f"window stream Ω {r['revision']}" for r in producer["stream"])
        assert all("window stream" not in text for text in producer["text"][0])
        assert after["vivid_overlay"]["windows"] == 1 and after["vivid_overlay"]["input_lanes"] == 2
        assert after["window"]["creation_index"] == before["window"]["creation_index"]
        sessions = msg("vivid", "sessions", "--window-id", window)
        assert {s["session_id"] for s in sessions["sessions"]} == {s["session_id"] for s in report["sessions"]["sessions"]}
        report["stream"] = {"before": before, "after": after, "observation": observation,
                            "producer": producer, "sessions_after": sessions}
        save()
        capture("survivor")
        request("focus", owner=1)
        msg("key", "B", "--route", "ui", "--window-id", window)
        msg("paste", "survivor Ω", "--route", "ui", "--window-id", window)
        wait(lambda s: s["keys"][1] > 0 and "survivor Ω" in "".join(s["text"][1]), "post-burst peer input")
        request("advance")
        wait(lambda s: s["presented"][1], "fresh post-burst peer receipt")
        capture("fresh")
    try:
        wait(lambda s: s["stage"] == "ready", "initial receipt admission", 30)
        msg("capabilities")
        windows = msg("list-windows")["windows"]
        assert len(windows) == 1
        window = windows[0]["window_id"]
        report["window_id"] = window
        report["sessions"] = msg("vivid", "sessions", "--window-id", window)
        report["surfaces"] = msg("vivid", "surfaces", "--window-id", window)
        report["tracks"] = msg("vivid", "tracks", "--window-id", window)
        surfaces = report["surfaces"]["surfaces"]
        tracks = report["tracks"]["tracks"]
        assert len(surfaces) == len(tracks) == 2
        assert len({s["session_id"] for s in surfaces}) == 2
        assert {(s["context_id"], s["surface_id"]) for s in surfaces} == {(1, 1)}
        assert {(t["context_id"], t["surface_id"], t["track_id"], t["channel_generation"])
                for t in tracks} == {(1, 1, 2, 1)}
        focus_native()
        initial = inspect()
        assert initial["vivid_overlay"]["windows"] == 2
        assert initial["vivid_overlay"]["input_lanes"] == 2
        capture("initial")
        # Startup or a native focus loss may have revoked the producer's first
        # focus request. Reassert the intended owner after the OS capture.
        request("focus", owner=0)
        msg("key", "A", "--route", "ui", "--window-id", window)
        msg("paste", "Ω", "--route", "ui", "--window-id", window)
        wait(lambda s: s["keys"][0] > 0 and "Ω" in "".join(s["text"][0]), "first input")
        assert state()["text"][1] == [] and state()["keys"][1] == 0
        if wide_winding:
            winding_work()
        elif work_recovery:
            recovery_work()
        elif stream_refusal:
            streaming_work()
        elif work_refusal:
            hostile_work()
        else:
            msg("set-visible", "--visible", "false", "--window-id", window)
            hidden_hold("explicit_hide")
            msg("set-visible", "--visible", "true", "--window-id", window)
            focus_native()
            wait(lambda s: all(s["presented"]), "restored original receipts")
            request("focus", owner=0)
            msg("paste", "restored Ω", "--route", "ui", "--window-id", window)
            wait(lambda s: "restored Ω" in "".join(s["text"][0]), "restored Unicode")
            restored = capture("restored")
            assert state()["text"][1] == []
            msg("resize", "--width", 1152, "--height", 784, "--window-id", window)
            msg("wait", "frame", "--after-frame", restored["frame_sequence"], "--timeout", "10s",
                "--window-id", window)
            resized = capture("resized")
            assert (resized["width"], resized["height"]) == (1152, 784), resized
            request("advance")
            wait(lambda s: all(s["presented"]), "resized original receipts")
            request("focus", owner=1)
            msg("key", "B", "--route", "ui", "--window-id", window)
            msg("paste", "peer Ω", "--route", "ui", "--window-id", window)
            wait(lambda s: s["keys"][1] > 0 and "peer Ω" in "".join(s["text"][1]), "peer input")
            assert "peer Ω" not in "".join(state()["text"][0])
            msg("key", "m", "--mods", "Super", "--route", "ui", "--window-id", window)
            until = time.monotonic() + 10
            while not inspect()["window"]["occluded"]:
                assert time.monotonic() < until, "normal minimize binding did not occlude"
                time.sleep(0.02)
            hidden_hold("minimized")
            msg("set-visible", "--visible", "true", "--window-id", window)
            focus_native()
            wait(lambda s: all(s["presented"]), "minimized restoration")
            request("focus", owner=1)
            msg("paste", "minimized Ω", "--route", "ui", "--window-id", window)
            wait(lambda s: "minimized Ω" in "".join(s["text"][1]), "minimized fresh Unicode")
            request("close_first")
            request("advance")
            wait(lambda s: s["presented"][1], "survivor original receipt")
            request("focus", owner=1)
            msg("paste", "survivor Ω", "--route", "ui", "--window-id", window)
            wait(lambda s: "survivor Ω" in "".join(s["text"][1]), "surviving peer")
            final = inspect()
            assert final["vivid_overlay"]["windows"] == 1
            capture("survivor")
        report["final_state"] = state()
        request("finish")
        process.wait(timeout=15)
        assert process.returncode == 0, "ordinary window did not retire cleanly"
        assert not (output / "producer-error.txt").exists(), "producer cleanup failed; see private error report"
        report["exit_code"] = 0
    except Exception as error:
        report["failure"] = {"type": type(error).__name__, "message": str(error)[:4096]}
        raise
    finally:
        if process.poll() is None:
            try:
                msg("quit")
                process.wait(timeout=10)
            except (AssertionError, subprocess.TimeoutExpired):
                process.terminate()
                process.wait(timeout=10)
        report["parent_exit_code"] = process.returncode
        save()
    return report


def main():
    root = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=root / "vivido/target/debug/vivido")
    parser.add_argument("--python", type=Path, default=root / "vivid_sdk/.venv/bin/python")
    parser.add_argument("--output-directory", type=Path)
    parser.add_argument("--work-refusal", action="store_true")
    parser.add_argument("--stream-refusal", action="store_true")
    parser.add_argument("--wide-winding", action="store_true")
    parser.add_argument("--work-recovery", choices=["hide", "minimize", "resize", "retain-hide", "retain-minimize", "retain-resize", "retain-downsize", "lease-resume", "lease-hide"])
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("this native window acceptance qualifies macOS only")
    output = args.output_directory or Path(tempfile.mkdtemp(prefix="vivido-window-parent-")) / "run"
    run(args.binary, args.python, output, args.work_refusal, args.stream_refusal, args.work_recovery, args.wide_winding)
    label = ("ordinary Vivido wide-winding coverage" if args.wide_winding else
             f"ordinary Vivido hostile-work {args.work_recovery} recovery" if args.work_recovery else
             "ordinary Vivido streaming refusal window" if args.stream_refusal else
             "ordinary Vivido work-refusal window" if args.work_refusal else "ordinary Vivido overlay window")
    print(f"PASS: {label}; evidence: {output}")


if __name__ == "__main__":
    main()
