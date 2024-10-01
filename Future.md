# Some ideas for future developments

1. More output options:
Use [cpal](https://crates.io/crates/cpal) or more likely [rodio](signal_after_end), and [pipewire](https://crates.io/crates/pipewire). These can be implemented either as compile-time features or run-time selectable by command line switch. cpal / rodio is likely to extend the possibility of using Vibe on other platforms but because pulseaudio is implemented *by* pipewire on modern systems, it's unlikely that pipewire will provide anything new, it **should** be implemented though if Vibe is to stay relevant.

2. Implement SSL.

3. Extract Icy metadata from the stream.

4. dbus integration. Show metadata and possibly control the player (although the LMS should be doing this).

5. Keep trying to get player sync in good working order.
