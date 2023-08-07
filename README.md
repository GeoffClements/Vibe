# Vibe

## About
Vibe is a music player that uses the [SLIM TCP protocol][`slimtcp`] to 
connect to a [Logitech Media Server][`lms`].

If you're looking for a well-tested, proven player then this is *not* it, 
instead you need [squeezelite][`squeezelite`] which has a robust, well-maintained
codebase and far more run-time and compile-time options than Vibe.

However, if you'd like to give Vibe a go then please do, it should be 
considered as beta code and any real-world testing is welcome.

Other than pulseaudio Vibe has zero run-time dependencies, all the stream
demultiplexing and codec decoding is done natively thanks to 
[Symphonia][`symphonia`], a big "thank-you" to the Symphonia devs for their
amazing work!.

## What Vibe can do
- Play Flac, AAC, Apple lossless, Ogg/Vorbis, MP3 and PCM streams
- Gapless playback
- Stop, play, pause and resume
- Volume control
- Play some radio streams

## What Vibe can't do
- Synchronise with other players (although it *should*, this is a WIP)
- Can't play some radio streams, but neither can my Squeezebox.

## Background
Vibe is 100% written in [Rust][`rust`] and has all the benefits that Rust
provides such as memory safety while being as performant as C. I wrote Vibe
as an exercise to practice writing a real application in Rust. If you enjoy
using it, please let me know. Equally please file any bug reports and lodge
any suggestions at [the home page](https://github.com/GeoffClements/Vibe).

[`slimtcp`]: https://wiki.slimdevices.com/index.php/SlimProto_TCP_protocol
[`lms`]: https://github.com/Logitech/slimserver
[`squeezelite`]: https://github.com/ralph-irving/squeezelite
[`symphonia`]: https://crates.io/crates/symphonia
[`rust`]: https://www.rust-lang.org/
