#![no_std]
#![no_main]

use core::fmt::Write;
use cyw43::{JoinOptions, aligned_bytes};
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, StackResources};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::Write as OtherWrite;
use heapless::String;
use panic_halt as _;
use static_cell::StaticCell;

const WIFI_NETWORK: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

const PORT_NUMBER: u16 = 8080;
static MUTEX: Mutex<ThreadModeRawMutex, u32> = Mutex::new(0);

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>;
});

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn dummy_task() {}

#[embassy_executor::main]
async fn main_task(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;
    let fw = aligned_bytes!("../../embassy/cyw43-firmware/43439A0.bin");
    let clm = aligned_bytes!("../../embassy/cyw43-firmware/43439A0_clm.bin");
    let nvram = aligned_bytes!("../../embassy/cyw43-firmware/nvram_rp2040.bin");

    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs download ../../cyw43-firmware/43439A0.bin --binary-format bin --chip RP235x --base-address 0x10100000
    //     probe-rs download ../../cyw43-firmware/43439A0_clm.bin --binary-format bin --chip RP235x --base-address 0x10140000
    // let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 230321) };
    // let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        // SPI communication won't work if the speed is too high, so we use a divider larger than `DEFAULT_CLOCK_DIVIDER`.
        // See: https://github.com/embassy-rs/embassy/issues/3960.
        RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    spawner.spawn(cyw43_task(runner).unwrap());

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = Config::dhcpv4(Default::default());
    let seed = rng.next_u64();

    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(net_task(runner).unwrap());

    if with_timeout(Duration::from_millis(30_000), async {
        loop {
            if control
                .join(WIFI_NETWORK, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
                .await
                .is_ok()
            {
                break;
            }
        }
    })
    .await
    .is_err()
    {
        // If the link fails an attempt, blink the LED once
        control.gpio_set(0, true).await;
        Timer::after(Duration::from_millis(500)).await;
        control.gpio_set(0, false).await;
    }

    stack.wait_link_up().await;
    stack.wait_config_up().await;

    // If the link succeeds, blink the LED twice and quickly
    control.gpio_set(0, true).await;
    Timer::after(Duration::from_millis(250)).await;
    control.gpio_set(0, false).await;
    Timer::after(Duration::from_millis(250)).await;
    control.gpio_set(0, true).await;
    Timer::after(Duration::from_millis(250)).await;
    control.gpio_set(0, false).await;

    // Unlike FreeRTOS with lwIP, in embassy-net *you* manage the rx and tx buffers
    // This makes multithreading more difficult because a TcpSocket borrows those buffers
    // With that said, it's possible to have priority-based tasking in embassy:
    // https://github.com/embassy-rs/embassy/blob/main/examples/rp235x/src/bin/multiprio.rs
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    let mut client_message_bytes = [0; 1024];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.accept(PORT_NUMBER).await.unwrap();
        socket.read(&mut client_message_bytes).await.unwrap();
        let client_message = str::from_utf8(&client_message_bytes).unwrap();
        let (request_line, _) = client_message.split_once("\r\n").unwrap();
        // Only accepting HTTP GET requests
        if !request_line.starts_with("GET") {
            continue;
        }

        // Where a separate thread would have started:

        let mut server_message = [0; 512];
        let mut content = [0; 128];

        if let Ok(mut guard) = with_timeout(Duration::from_millis(10), MUTEX.lock()).await {
            *guard += 1;
        }

        const REQUEST_LINE: &[u8; 17] = b"HTTP/1.1 200 OK\r\n";
        const REQUEST_LINE_SIZE: usize = size_of_val(REQUEST_LINE);
        server_message
            .first_chunk_mut::<REQUEST_LINE_SIZE>()
            .unwrap()
            .copy_from_slice(REQUEST_LINE);

        let (_, rest) = server_message
            .split_first_chunk_mut::<REQUEST_LINE_SIZE>()
            .unwrap();
        const SERVER: &[u8; 31] = b"Server: Raspberry Pi Pico 2 W\r\n";
        const SERVER_SIZE: usize = size_of_val(SERVER);
        rest.first_chunk_mut::<SERVER_SIZE>()
            .unwrap()
            .copy_from_slice(SERVER);
        let (_, rest) = rest.split_first_chunk_mut::<SERVER_SIZE>().unwrap();
        const LAST_MODIFIED: &[u8; 46] = b"Last-Modified: Thu, 18 Mar 2026 05:35:18 GMT\r\n";
        const LAST_MODIFIED_SIZE: usize = size_of_val(LAST_MODIFIED);
        rest.first_chunk_mut::<LAST_MODIFIED_SIZE>()
            .unwrap()
            .copy_from_slice(LAST_MODIFIED);
        let (_, rest) = rest.split_first_chunk_mut::<LAST_MODIFIED_SIZE>().unwrap();
        const ACCEPT_RANGES: &[u8; 22] = b"Accept-Ranges: bytes\r\n";
        const ACCEPT_RANGES_SIZE: usize = size_of_val(ACCEPT_RANGES);
        rest.first_chunk_mut::<ACCEPT_RANGES_SIZE>()
            .unwrap()
            .copy_from_slice(ACCEPT_RANGES);

        const CONTENT_INTRO: &[u8; 70] =
            b"What's up? This server was written in no_std Rust. Your IP address is ";
        const CONTENT_INTRO_SIZE: usize = size_of_val(CONTENT_INTRO);
        content
            .first_chunk_mut::<CONTENT_INTRO_SIZE>()
            .unwrap()
            .copy_from_slice(CONTENT_INTRO);

        // VERY ANNOYING FORMATTING ISSUE, THANK YOU SO MUCH SMOLTCP
        let mut endpoint = String::<16>::new();
        write!(endpoint, "{}", socket.remote_endpoint().unwrap().addr).unwrap();

        let mut ip_addr = String::<18>::new(); // 3 off due to null byte and \r\n
        const IPADDR_SIZE: usize = 15;
        write!(
            ip_addr,
            "{:<width$}\r\n",
            endpoint.as_str(),
            width = IPADDR_SIZE
        )
        .unwrap(); // adds padding spaces to fit
        let (_, rest_content) = content
            .split_first_chunk_mut::<CONTENT_INTRO_SIZE>()
            .unwrap();
        rest_content
            .first_chunk_mut::<{ IPADDR_SIZE + 2 }>()
            .unwrap()
            .copy_from_slice(ip_addr.as_bytes());
        const CONTENT_SIZE: usize = CONTENT_INTRO_SIZE + IPADDR_SIZE + 2;

        let (_, rest) = rest.split_first_chunk_mut::<ACCEPT_RANGES_SIZE>().unwrap();
        const CONTENT_LENGTH: &[u8; 16] = b"Content-Length: ";
        const CONTENT_LENGTH_SIZE: usize = size_of_val(CONTENT_LENGTH);
        rest.first_chunk_mut::<CONTENT_LENGTH_SIZE>()
            .unwrap()
            .copy_from_slice(CONTENT_LENGTH);
        let (_, rest) = rest.split_first_chunk_mut::<CONTENT_LENGTH_SIZE>().unwrap();
        let mut eighth = String::<4>::new(); // one off due to null byte
        const CONTENT_LENGTH_NUM_SIZE: usize = 3;
        write!(
            eighth,
            "{:<width$}",
            CONTENT_SIZE,
            width = CONTENT_LENGTH_NUM_SIZE
        )
        .unwrap(); // adds padding spaces to fit
        rest.first_chunk_mut::<CONTENT_LENGTH_NUM_SIZE>()
            .unwrap()
            .copy_from_slice(eighth.as_bytes());
        let (_, rest) = rest
            .split_first_chunk_mut::<CONTENT_LENGTH_NUM_SIZE>()
            .unwrap();
        const CONTENT_TYPE: &[u8; 29] = b"\r\nContent-Type: text/html\r\n\r\n"; // including trailing \r\n's for convenience
        const CONTENT_TYPE_SIZE: usize = size_of_val(CONTENT_TYPE);
        rest.first_chunk_mut::<CONTENT_TYPE_SIZE>()
            .unwrap()
            .copy_from_slice(CONTENT_TYPE);
        let (_, rest) = rest.split_first_chunk_mut::<CONTENT_TYPE_SIZE>().unwrap();
        rest.first_chunk_mut::<CONTENT_SIZE>()
            .unwrap()
            .copy_from_slice(content.first_chunk::<CONTENT_SIZE>().unwrap());

        const TOTAL_SIZE: usize = REQUEST_LINE_SIZE
            + SERVER_SIZE
            + LAST_MODIFIED_SIZE
            + ACCEPT_RANGES_SIZE
            + CONTENT_LENGTH_SIZE
            + CONTENT_LENGTH_NUM_SIZE
            + CONTENT_TYPE_SIZE
            + CONTENT_SIZE;
        socket
            .write_all(server_message.first_chunk::<TOTAL_SIZE>().unwrap())
            .await
            .unwrap();
        socket.flush().await.unwrap();
        socket.close();
    }
}
