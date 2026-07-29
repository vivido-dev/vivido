#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
vivido_root=$(cd -- "$script_dir/.." && pwd)
vivi_root=$(cd -- "$vivido_root/../vivi" && pwd)
tmp_dir=$(mktemp -d)
socket_path="$tmp_dir/vivido.sock"
config_path="$tmp_dir/vivido.toml"
image_path="$tmp_dir/red.png"
log_path="$tmp_dir/vivido.log"
screenshot_path=
vivido_pid=

cleanup() {
    if [[ -n "$vivido_pid" ]] && kill -0 "$vivido_pid" 2>/dev/null; then
        kill "$vivido_pid" 2>/dev/null || true
        wait "$vivido_pid" 2>/dev/null || true
    fi
    if [[ -n "$screenshot_path" ]]; then
        rm -f -- "$screenshot_path"
    fi
    rm -rf -- "$tmp_dir"
}
trap cleanup EXIT INT TERM

cd "$vivido_root"
cargo build --bin vivido
cargo build --manifest-path "$vivi_root/Cargo.toml" --bin vivi
vivido_bin="$vivido_root/target/debug/vivido"
vivi_bin="$vivi_root/target/debug/vivi"

# Avoid user configuration and create a deterministic opaque background.
printf '%s\n' '[window]' 'opacity = 1.0' >"$config_path"

python3 - "$image_path" <<'PY'
import binascii
import struct
import sys
import zlib

path = sys.argv[1]
width, height = 64, 64
raw = b"".join(b"\0" + bytes((255, 0, 0, 255)) * width for _ in range(height))

def chunk(kind, payload):
    return (
        struct.pack(">I", len(payload))
        + kind
        + payload
        + struct.pack(">I", binascii.crc32(kind + payload) & 0xFFFFFFFF)
    )

png = (
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    + chunk(b"IDAT", zlib.compress(raw))
    + chunk(b"IEND", b"")
)
with open(path, "wb") as output:
    output.write(png)
PY

unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET MIR_SOCKET
"$vivido_bin" \
    --headless \
    --daemon \
    --config-file "$config_path" \
    --socket "$socket_path" \
    >"$log_path" 2>&1 &
vivido_pid=$!

for _ in {1..200}; do
    if "$vivido_bin" msg --socket "$socket_path" capabilities >/dev/null 2>&1; then
        break
    fi
    if ! kill -0 "$vivido_pid" 2>/dev/null; then
        echo "headless Vivido exited before IPC became ready" >&2
        exit 1
    fi
    sleep 0.025
done
"$vivido_bin" msg --socket "$socket_path" capabilities >/dev/null

window_id=$(
    "$vivido_bin" msg --socket "$socket_path" create-window \
        --dimensions 80x24 \
        --command /bin/sh
)

printf -v command_text '%q %q && printf "\\n__VIVI_PRESENTED__\\n"' "$vivi_bin" "$image_path"
"$vivido_bin" msg --socket "$socket_path" typing \
    --window-id "$window_id" \
    "$command_text"
"$vivido_bin" msg --socket "$socket_path" key Enter --window-id "$window_id"

# Vivi returns only after its Vivid WAIT_FIRST_VISIBLE_PRESENTATION succeeds.
"$vivido_bin" msg --socket "$socket_path" wait text __VIVI_PRESENTED__ \
    --timeout 30s \
    --window-id "$window_id" \
    >/dev/null

screenshot_path=$(
    "$vivido_bin" msg --socket "$socket_path" screenshot --window-id "$window_id"
)

python3 - "$screenshot_path" <<'PY'
import struct
import sys
import zlib

data = open(sys.argv[1], "rb").read()
if data[:8] != b"\x89PNG\r\n\x1a\n":
    raise SystemExit("screenshot is not a PNG")

offset = 8
compressed = bytearray()
width = height = color_type = None
while offset < len(data):
    length = struct.unpack(">I", data[offset:offset + 4])[0]
    kind = data[offset + 4:offset + 8]
    payload = data[offset + 8:offset + 8 + length]
    offset += 12 + length
    if kind == b"IHDR":
        width, height, depth, color_type, _, _, interlace = struct.unpack(">IIBBBBB", payload)
        if depth != 8 or color_type != 6 or interlace != 0:
            raise SystemExit("unexpected screenshot PNG format")
    elif kind == b"IDAT":
        compressed.extend(payload)
    elif kind == b"IEND":
        break

raw = zlib.decompress(compressed)
stride = width * 4
rows = []
previous = bytearray(stride)
cursor = 0

def paeth(a, b, c):
    estimate = a + b - c
    pa, pb, pc = abs(estimate - a), abs(estimate - b), abs(estimate - c)
    return a if pa <= pb and pa <= pc else b if pb <= pc else c

for _ in range(height):
    filter_type = raw[cursor]
    cursor += 1
    encoded = raw[cursor:cursor + stride]
    cursor += stride
    row = bytearray(stride)
    for index, value in enumerate(encoded):
        left = row[index - 4] if index >= 4 else 0
        above = previous[index]
        upper_left = previous[index - 4] if index >= 4 else 0
        if filter_type == 0:
            decoded = value
        elif filter_type == 1:
            decoded = value + left
        elif filter_type == 2:
            decoded = value + above
        elif filter_type == 3:
            decoded = value + ((left + above) // 2)
        elif filter_type == 4:
            decoded = value + paeth(left, above, upper_left)
        else:
            raise SystemExit("unsupported PNG row filter")
        row[index] = decoded & 0xFF
    rows.append(row)
    previous = row

red_pixels = 0
for row in rows:
    for index in range(0, len(row), 4):
        red, green, blue, alpha = row[index:index + 4]
        if red >= 220 and green <= 40 and blue <= 40 and alpha >= 220:
            red_pixels += 1

if red_pixels < 256:
    raise SystemExit(f"fresh screenshot contains only {red_pixels} expected red media pixels")
PY

echo "headless Vivi smoke test passed"
