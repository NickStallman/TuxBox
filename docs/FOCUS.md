# Where to improve

Open areas that need work, with what is known and a useful next step. [Progress](PROGRESS.md) records what works today. If you start on one, say so in an issue so effort isn't doubled. The [roadmap](ROADMAP.md) breaks shader translation and a project-wide audit into numbered tasks. Measurements below are from ProjectScarlett 1.0.2.1 on a Ryzen with 16 threads, 32 GB of memory and an RTX 4070 Ti SUPER, unless noted.

## Memory

Running out of memory on the way into the game is a main blocker. The console is one chip with a single pool of shared memory; a PC splits it between system memory and the graphics card, and adds the translation layer, driver and compilers on top.

- **Give games the console's budget.** A Series X leaves a game 13.5 GB, and TuxBox should report that as a first step. Today `XMemGetWorkingSetStatistics` reports a budget based on the whole PC, and counts the whole process as used, so lowering the budget alone could tell a game it is already full. Report the budget with the game's own allocations as used, make `XMemGetFreePageStatistics` agree, and check what the game does with `GlobalMemoryStatusEx`.
- **Find who owns the memory.** On the loading art the game process had 16 to 20 GB in swap and 1 to 6 GB resident, with about 10 GB of graphics memory in use. Memory moved to swap while over 11 GB was still free; why is unknown. On November 27, a launch left on an early screen reached 28.5 GB of memory and swap after about three and a half minutes, with only 30 MB of copied textures. 22.8 GB sat in 64 MB blocks laid out like the Linux C library's allocator, and grew while the game created pipelines. The driver's pipeline compiler is suspected; nothing has shown it.
- **Copied textures.** On a 12 GB card, most of the PC copies of game textures were never filled with data ([#292](https://github.com/brcly/TuxBox/issues/292)). Giving a copy memory only when it is first filled or written would recover that, and is untested.
- **Overflow to disk.** Investigate a pagefile that TuxBox sets up for itself and the game, for what doesn't fit the budget. Creating Linux swap needs administrator rights, so it must not happen silently; backing game memory with a file is untested and the graphics driver may refuse it. A disk swap file set up by the person is the stopgap. A host with only zram has nowhere to spill.
- **Graphics memory runs out entering the world.** In a November 27 launch on a 16 GB card, graphics memory went from 9.6 GB to 15.6 GB within about 30 seconds. Allocations fell back to system memory, available system memory fell to under 3 GB, and the GPU faulted 13 seconds after the first failed allocation ([record](../evidence/projectscarlett-memory-run.json)). A later launch that kept upload memory out of graphics memory had no failed allocations and hit the same fault, so running out of graphics memory isn't required. A similar order has been reported on RADV. The title still had not asked for its memory budget.
- **In-world performance** will need work on how shared console memory maps onto a PC, once a game gets that far.

## Startup and shader compilation

A warm launch reached player selection in 69 s. Cold launches take many minutes, and it is too slow. The first launch, with nothing cached, matters most, and fixes should help any title.

- **Measure each stage.** A shader goes from console code to HLSL, DXC (DXIL), dxil-spirv in vkd3d-proton, then the driver. Compare time and memory per stage for a cold and a warm start.
- **Make DXC faster.** Try its settings, or our own build of it. Lifetime markers are now off for every shader. With the simpler branch and mesh code, 800 random ProjectScarlett programs compiled 1.26 times as fast offline: 3.9 times for `ms_6_6`, but only 1.05 times for `cs_6_6`, half of the total. A DXC that lists `-fnew-inlining-behavior` now gets it, which compiled 800 programs 1.31 times as fast. Splitting compute shaders into small functions made DXC about 3.4 times as fast on 12 large ones, but dxil-spirv could not convert the split shaders that otherwise built. No launch gain has been measured.
- **Benchmark a shorter route.** On Linux, translating straight to SPIR-V would drop DXC and dxil-spirv. Windows would keep DXIL. Compare both against the per-stage baseline before building either.
- **Measure speed in the checks.** A change that alters compile speed without changing coverage should show in `tuxbox.py check`, with outputs still verified.
- **Fewer empty stand-ins.** A large compile that passes its deadline is replaced by an empty pipeline for the whole session. Swapping in the finished result later is untested. Lowering the number of large compiles with available memory went with hundreds more stand-ins in one cold launch; the cause was not isolated.
- **Large compute shaders.** Single driver compiles took up to 158 s. Smaller or better structured translated code would help every stage.
- **Later:** translate the shaders seen in earlier launches on all cores before the game asks.

## Pipeline and driver caches

- The NVIDIA driver stopped updating its cache index once one file grew past about 5 GB, so every launch recompiled the same pipelines. TuxBox now limits the cache to 4 GiB. Check that long sessions stay warm, and find the real limit.
- A new translator version retranslates shaders on the next launch. Measure how much of that cost can be avoided.

## Audio start-up

ProjectScarlett's presenter shares a lock with its audio output. When audio start-up was slow, the game's own code left that lock held and froze presentation; TuxBox now patches that. The slow start came from a failing USB audio device on the host: while it was enabled, streams timed out and piled up.

- Find out why one failing device stalls streams on another output, and whether Wine can recover or report it.
- Check whether the black screen before player selection is the splash screen that should be drawn there.

## Frame pacing

At high frame rates ProjectScarlett shows pairs of frame time spikes between otherwise even frames. Capture them with `TUXBOX_FRAME_LOG` or `--mangohud` and find what runs at those moments.

## Shader translation

Compile time comes first (above), then unsupported shaders; a larger change with some risk is acceptable if it is verified. Preparation on November 26 compiled 36,653 of 41,064 recorded requests ([refusals](SHADERS.md#projectscarlett-refusals)). That count overstates what a live run loses. In a launch the same day, the causes that blocked the most pipelines were group sums in divergent control flow (43), lane reads inside non-uniform loops (24), waves that must be complete (15) and unrecognized mesh launch calculations (15).

- Many pipelines still get empty stand-ins because a stage can't be translated. Rank them with the [pipeline failure record](PROJECTSCARLETT.md#finding-why-pipelines-fail), by pipelines blocked rather than by refused requests. A refusal for a missing buffer stride is often a first attempt that a retry translates.
- Check each instruction against AMD's RDNA 2 instruction set reference, which is also published as XML with its developer tools. AMD's Radeon GPU Analyzer shows what a pattern compiles to; the offline Vulkan compiler in version 2.15 does not target the console's generation. Neither proves a translation correct: compare its output on the GPU with a reference.
- Group sums in divergent control flow need a proof, or a vote, that every lane of a wave takes the same path.
- dxil-spirv reports `Failed to remap` errors for some translated shaders. Find which bindings cause them.
- The two-lanes-per-thread translation for 64-lane programs passes synthetic tests. Its output hasn't been verified in a game.

## AMD and NVIDIA

- RADV substitutes other memory for imported game pages, and TuxBox copies them instead, which is slow and loses GPU writes ([#191](https://github.com/brcly/TuxBox/issues/191)).
- Shared code needs a run on both vendors. See [CONTRIBUTING](../CONTRIBUTING.md#amd-and-nvidia).

## GPU synchronisation

GPU queues stall on fence label waits at ProjectScarlett's story intro, and the title's watchdog ends the run ([#215](https://github.com/brcly/TuxBox/issues/215)).

## Keeping the ground rules

- Linux comes first, and changes must keep Windows working.
- Don't patch Proton. If its behaviour must change, hook it while the game runs, or look for another route.
