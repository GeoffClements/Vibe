# Vibe

## About
Vibe is a music player that uses the [SLIM TCP protocol][`slimtcp`] to 
connect to a [Lyrion Music Server][`lms`] formally known as a
Logitech Media Server.

Vibe is intended to be run as a user daemon and designed to be simple and
as unobtrusive as possible.

I use Vibe as my daily driver, so it gets lots of use by me and I fix
bugs as I find them, but this is on a linux system and using the 
`pulseaudio` output. For anything else, I rely on bug reports but
I'm limited to testing on a Linux system only.

## Running
```
vibe -h
```
This will provide the options you can use.

There is a systemd service file in the resources directory
which you can adapt to your needs.

Once compiled, move the `vibe` executable to where you want on your
system then edit the `vibe_daemon.service` file so that the
`ExecStart=` line points to it, then add options, if any, to the 
vibe daemon.

Copy the systemd service file to `~/.config/systemd/user/` and then
tell systemd of the new service with
```bash
systemctl --user daemon-reload
```
You only need to do this once.

Start the service with
```bash
systemctl --user start vibe_daemon.service
```

You can make it so that vibe will start whenever you login with
```bash
systemctl --user enable vibe_daemon.service
```


## Output
By default, Vibe uses the `pulseaudio` API which means that it can play
sounds both with the `pulseaudio` system and with the `pipewire` system, thanks
to the fact that `pipewire` implements the `pulseaudio` API.

There is also the compile-time option `rodio`, for playing audio via other systems
such as ALSA. This uses the `rodio` crate which, in turn, uses the `cpal` crate.
This means that the following hosts are possible:
- Linux (via ALSA or JACK)
- Windows (via WASAPI by default, see ASIO instructions below)
- macOS (via CoreAudio)
- iOS (via CoreAudio)
- Android (via Oboe)
- Emscripten

However, even if the `rodio` feature is selected, Vibe still has a dependency
on `pulseaudo` and this might prevent it being used on other platforms.

## Compilation

### Compile-time Dependencies
Vibe needs the `pulseaudio` development files. These are provided as
part of the `libpulse-dev` package on Debian and Ubuntu distributions.

If the `rodio` feature is selected, then Vibe also needs
the ALSA development files. These are provided as part of the libasound2-dev
package on Debian and Ubuntu distributions and alsa-lib-devel on Fedora.

### Features
Symphonia has optimization features that are off by default, you can switch them on 
with `--features symphonia/<optimisation>`. These features are:
 - `opt-simd-sse`
 - `opt-simd-avx`
 - `opt-simd-neon`

or you can switch them all on with `opt-simd`.

If the Symphonia devs have them off by default then so will I.

To use `rodio`/`cpal`, use the `rodio` feature. This will add the 
ability to use `--system=rodio` on the command line. Note that when
this feature flag is used, Vibe will still default to `pulseaudio`.

## Dependencies
Vibe has zero run-time dependencies, all the stream
demultiplexing and codec decoding is done natively thanks to 
[Symphonia][`symphonia`], a big "thank-you" to the Symphonia devs for their
amazing work!


[`slimtcp`]: https://wiki.slimdevices.com/index.php/SlimProto_TCP_protocol
[`lms`]: https://lyrion.org/
[`squeezelite`]: https://github.com/ralph-irving/squeezelite
[`symphonia`]: https://crates.io/crates/symphonia
[`rust`]: https://www.rust-lang.org/
[`rodio`]: https://crates.io/crates/rodio
[`cpal`]: https://crates.io/crates/cpal