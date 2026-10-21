# Dolby Vision RPU fixtures

`mel_orig.bin` and `fel_orig.bin` are single-frame profile 7 RPUs (MEL and FEL)
from the test assets of [dovi_tool](https://github.com/quietvoid/dovi_tool)
2.3.4, MIT licensed (see `LICENSE-dovi_tool.txt`). `tests/dovi.rs` repeats them
for every frame of a synthetic dual-layer sample. Profile 5 and 8 RPUs are
generated at test time with `dovi_tool generate`, different on every frame.
