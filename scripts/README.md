Scripts
=======

## Linux headless Vivid smoke test

Build Vivido and Vivi, start a display-independent Vulkan presenter, submit a generated image
through the inherited Vivid endpoint, wait for presentation, and validate a fresh screenshot:

```sh
./headless-vivi-smoke.sh
```

The test requires Linux, a Vulkan adapter (Mesa lavapipe is sufficient), Python 3, and the normal
Vivido/Vivi build dependencies. It never prints inherited Vivid endpoint credentials.

## Flamegraph

Run the release version of Vivido while recording call stacks. After the
Vivido process exits, a flamegraph will be generated and it's URI printed
as the only output to STDOUT.

```sh
./create-flamegraph.sh
```

Running this script depends on an installation of `perf`.

## ANSI Color Tests

We include a few scripts for testing the color of text inside a terminal. The
first shows various foreground and background variants. The second enumerates
all the colors of a standard terminal. The third enumerates the 24-bit colors.

```sh
./fg-bg.sh
./colors.sh
./24-bit-color.sh
```
