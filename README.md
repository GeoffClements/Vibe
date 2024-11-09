# Vibe

## About
Vibe is a music player that uses the [SLIM TCP protocol][`slimtcp`] to 
connect to a [Lyrion Music Server][`lms`] formally known as a
Logitech Media Server.

If you're looking for a well-tested, proven player then this is *not* it, 
instead you need [squeezelite][`squeezelite`] which has a robust, well-maintained
codebase and far more run-time and compile-time options than Vibe.

However, if you'd like to give Vibe a go then please do, it should be 
considered as beta code and any real-world testing is welcome.

## Output
This is the `master` branch of Vibe which means that it outputs its sounds
via `pulseaudio`. Because `pipewire` implements the `pulseaudio`
API, Vibe can also output sounds via `pipewire`.

 See the `rodio` branch for output via
[rodio][`rodio`] / [cpal][`cpal`].

On Linux Vibe needs the `pulseaudio` development files. These are provided as
part of the `libpulse-dev` package on Debian and Ubuntu distributions.

## Dependencies
Vibe has zero run-time dependencies, all the stream
demultiplexing and codec decoding is done natively thanks to 
[Symphonia][`symphonia`], a big "thank-you" to the Symphonia devs for their
amazing work!

Symphonia has optimisation features that are off by default, you can switch them on 
with `--features symphonia/<optimisation>`. These features are:
 - `opt-simd-sse`
 - `opt-simd-avx`
 - `opt-simd-neon`

or you can switch them all on with `opt-simd`.

## What Vibe can do
- Play Flac, AAC, Apple lossless, Ogg/Vorbis, MP3 and PCM streams
- Gapless playback when possible
- Stop, play, pause and resume
- Volume control
- Select output device
- Choose the name of the player
- Play some radio streams

## What Vibe can't do
- Synchronise with other players (although it *should*, this is a WIP).

## Background
Vibe is 100% written in [Rust][`rust`] and has all the benefits that Rust
provides such as memory safety while being as performant as C. I wrote Vibe
as an exercise to practice writing a real application in Rust. If you enjoy
using it, please let me know. Equally please file any bug reports and lodge
any suggestions at [the home page](https://github.com/GeoffClements/Vibe).

[`slimtcp`]: https://wiki.slimdevices.com/index.php/SlimProto_TCP_protocol
[`lms`]: https://lyrion.org/
[`squeezelite`]: https://github.com/ralph-irving/squeezelite
[`symphonia`]: https://crates.io/crates/symphonia
[`rust`]: https://www.rust-lang.org/
[`rodio`]: https://crates.io/crates/rodio
[`cpal`]: https://crates.io/crates/cpal