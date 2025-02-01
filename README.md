![Static Badge](https://img.shields.io/badge/Written%20in-Rust-blue?style=flat&logo=rust)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
![Static Badge](https://img.shields.io/badge/Feel%20the-Vibe-red?style=plastic)


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
To list the run-time options:
```
vibe -h
```

To see all audio output devices on your machine:
```
vibe -l
```

There is a systemd service file in the resources directory
which you can adapt to your needs as follows:

Once compiled, move the `vibe` executable to where you want on your
system then edit the `vibe_daemon.service` file so that the
`ExecStart=` line points to it, then add options you want, if any, to the 
vibe command.

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
By default, Vibe uses the `pulse` feature flag which means it uses 
the `pulseaudio` API. This means that it can play
sounds both with the `pulseaudio` system and with the `pipewire` system, thanks
to the fact that `pipewire` implements the `pulseaudio` API.

There is also the compile-time option `rodio`, for playing audio via other systems
such as ALSA. This uses the `rodio` crate which, in turn, uses the `cpal` crate.
This means that the following hosts are possible (according to `cpal` 
documentation):
- Linux (via ALSA or JACK)
- Windows (via WASAPI by default, also ASIO)
- macOS (via CoreAudio)
- iOS (via CoreAudio)
- Android (via Oboe)
- Emscripten

## Compilation

### Compile-time dependencies
Vibe needs the `pulseaudio` development files, but this can be disabled, see below.
These are provided as
part of the `libpulse-dev` package on Debian and Ubuntu distributions.

If the `rodio` feature is selected, then Vibe also needs
the ALSA development files. These are provided as part of the `libasound2-dev`
package on Debian and Ubuntu distributions and `alsa-lib-devel` on Fedora.

### Features
#### Symphonia optimization
Symphonia has optimization features that are off by default, you can switch them on 
with `--features symphonia/<optimization>`. These features are:
 - `opt-simd-sse`
 - `opt-simd-avx`
 - `opt-simd-neon`

or you can switch them all on with `opt-simd`.

If the Symphonia devs have them off by default then so will I.

#### Rodio
To use `rodio`/`cpal`, use the `rodio` feature. This will add the 
ability to use `--system=rodio` on the command line to select the 
rodio output. Note that when
this feature flag is used, Vibe will still default to `pulseaudio`
if `--system=rodio` is not specified.

When the `rodio` feature is selected, Vibe will compile for
both pulseaudio and rodio so will still have a dependency
on `pulseaudo`.
If you want to compile without having a dependency on pulseaudio then use
the `--no-default-features` compilation option. In this case, the rodio
feature flag must be selected, otherwise Vibe will not compile.

#### Notify
To enable new track notifications on the desktop use the `notify`
feature. If this is enabled, you will be able to suppress the
notifications with the `--quiet` command option.

To compile when this feature is selected, `pkg-config` must be
installed along with the dbus development package; this is
`libdbus-1-dev` on Debian and Ubuntu.

## Run-time Dependencies
Vibe has zero run-time dependencies, all the stream
demultiplexing and decoding is done natively thanks to 
[Symphonia][`symphonia`], a big "thank-you" to the Symphonia devs for their
amazing work!

## Support me
Vibe runs on coffee!
[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/V7V719WYF6)


[`slimtcp`]: https://wiki.slimdevices.com/index.php/SlimProto_TCP_protocol
[`lms`]: https://lyrion.org/
[`squeezelite`]: https://github.com/ralph-irving/squeezelite
[`symphonia`]: https://crates.io/crates/symphonia
[`rust`]: https://www.rust-lang.org/
[`rodio`]: https://crates.io/crates/rodio
[`cpal`]: https://crates.io/crates/cpal