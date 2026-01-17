sudo apt-get update
sudo apt-get install -y --no-install-recommends \
    libpulse-dev \
    libasound2-dev \
    libdbus-1-dev \
    pkg-config \
    libpipewire-0.3-dev \
    libspa-0.2-dev \
    libclang-dev

cargo install cargo-deb