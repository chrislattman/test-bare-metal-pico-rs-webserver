# Raspberry Pi Pico 2 running no_std Rust

This is essentially a bare metal version of https://github.com/chrislattman/webserver/blob/master/server.rs

While this repo implements its own HTTP server, a popular no_std Rust web server is picoserve.

Instructions:

- Follow the instructions at https://github.com/chrislattman/test-bare-metal-pico to install `picotool` if you haven't already
- Run `rustup target add thumbv8m.main-none-eabihf`
- Clone https://github.com/embassy-rs/embassy (this is because their releases on crates.io are lagging behind) as a sibling directory to this one
- You need to set the `WIFI_SSID` and `WIFI_PASSWORD` environment variables for this example to work

To build application and run on board:

- Unplug USB cable from board
- Hold down BOOTSEL button while plugging in USB cable
- Run `cargo run --release`

Note: this example does NOT work for the Raspberry Pi Pico 2, as that board doesn't have a CYW43439 Wi-Fi/Bluetooth chip.
