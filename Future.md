# Some ideas for future developments

1. More output options:
Use [cpal](https://crates.io/crates/cpal) and [pipewire](https://crates.io/crates/pipewire). These can be implemented either as compile-time features or run-time selectable by command line switch. cpal is likely to extend the possibility of using Vibe on other platforms but because pulseaudio is implemented *by* pipewire on modern systems, it's unlikely that pipewire will provide anything new, it **should** be implemented though if Vibe is to stay relevant.

2. Implement SSL.

3. Extract Icy metadata from the stream.

4. Keep trying to get player sync in good working order.
