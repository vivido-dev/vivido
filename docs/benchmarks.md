# Terminal Benchmarks

## Terminal throughput comparison

Measured with `kitten __benchmark__` [source](https://github.com/kovidgoyal/kitty/blob/master/tools/cmd/benchmark/main.go).

It works by dumping large amounts of data of different types into the tty device and measuring how fast the terminal parses and responds to it.

Results in MB/s — higher is better.

| Terminal  | ASCII | Unicode | CSI codes | Long escape codes | Images  | Average |
|-----------|-------|---------|-----------|-------------------|---------|---------|
| Vivido    | 133.8 | 186.3   | 98.0      | **517.0**         | 394.8   | **265.98**  |
| Kitty     | 83.4  | 148.4   | 69.3      | 325.3             | 291.6   | 183.6   |
| Ghostty   | 93.2  | 133.8   | 49.5      | 96.6              | 73.6    | 89.34   |
| iTerm2    | 19.2  | 23.9    | 2.2       | 76.5              | 31.4    | 30.64   |
| Tabby     | 19.0  | 20.9    | 11.4      | 25.3              | 24.7    | 20.26   |
| Orca      | 12.0  | 32.0    | 7.4       | 13.4              | 13.4    | 15.64   |
| Alacritty | **157.5** | **220.4** | **105.1** | 247.2 | **450.2** | 236.08  |
