# Linux baseline before the GPUI migration

Measured on the packaged Linux ARM64 artifact of `8053804` on a Snapdragon X Elite laptop. The test used Xvfb at 1280 × 900, Mesa software Vulkan, ten fresh empty homes, a warm page cache and no account. It launched the executable with an explicit project directory and polled for a visible X11 window with the process ID and title `OX Desktop`.

| Observable | Result |
| --- | --- |
| Process launch to mapped window, ten runs (ms) | 110.9, 105.2, 99.9, 97.8, 104.9, 98.1, 104.7, 98.4, 103.6, 105.6 |
| Ninth of ten launch times | 105.6 ms |
| GUI process RSS / PSS after two seconds on the final launch | 218532 / 193934 KiB |
| GUI process CPU over the following ten seconds | 0.3% of one core |
| Packaged GUI executable | 25898624 bytes |

The start-up observation ends when X11 reports the window mapped. The wave 2 presentation gate ends on the first presented frame with a populated profile. Software-renderer allocations contribute to RSS in this measurement. Use the same renderer and profile when comparing runs.
