#![no_std]
#![no_main]

mod board;
mod dap_usb;
mod protocol;
pub mod swd;
mod wiring;

use board::Usb;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_futures::select::{Either, select};
use embassy_stm32::can::{CanRx, CanTx};
use embassy_stm32::usart::BufferedUart;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::{
    Builder,
    class::cdc_acm::{CdcAcmClass, Receiver, Sender, State},
};
use embedded_io_async::{Read, Write};
use panic_halt as _;
use protocol::{ClassicFrame, HostCommand, ParseError, Reply};
use static_cell::StaticCell;
use swd::Swd;

const PACKET: usize = 64;

// Bounded queues absorb bursts. CAN reception is allowed to drop (with a
// counter) rather than wait for a slow or disconnected USB host.
const CAN_TO_HOST_DEPTH: usize = 24;
const HOST_TO_CAN_DEPTH: usize = 8;
const REPLY_DEPTH: usize = 8;

static CAN_TO_HOST: Channel<CriticalSectionRawMutex, ClassicFrame, CAN_TO_HOST_DEPTH> =
    Channel::new();
static HOST_TO_CAN: Channel<CriticalSectionRawMutex, ClassicFrame, HOST_TO_CAN_DEPTH> =
    Channel::new();
static REPLIES: Channel<CriticalSectionRawMutex, Reply, REPLY_DEPTH> = Channel::new();

static RX_TOTAL: AtomicU32 = AtomicU32::new(0);
static TX_TOTAL: AtomicU32 = AtomicU32::new(0);
static DROP_RX: AtomicU32 = AtomicU32::new(0);
static DROP_TX: AtomicU32 = AtomicU32::new(0);

// Track linker-script changes so Cargo relinks the firmware.
const _: &[u8] = include_bytes!("../memory.x");

enum HostTxItem {
    Reply(Reply),
    Frame(ClassicFrame),
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let (driver, ports, swd) = wiring::configure(board::init());
    // Classroom-only test VID/PID, not allocated product identifiers.
    let mut config = embassy_usb::Config::new(0xC0DE, 0xCAFC);
    config.manufacturer = Some("HerKules");
    config.product = Some("DM-MC02 CMSIS-DAP Demo");
    config.max_packet_size_0 = PACKET as u8;
    static DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS: StaticCell<[u8; 128]> = StaticCell::new();
    static CONTROL: StaticCell<[u8; 64]> = StaticCell::new();
    static CTL_STATE: StaticCell<State> = StaticCell::new();
    static UART_STATE: StaticCell<State> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        DESC.init([0; 256]),
        BOS.init([0; 128]),
        MSOS.init([0; 256]),
        CONTROL.init([0; 64]),
    );
    builder.msos_descriptor(0x0603_0000, 0x20);
    let dap = dap_usb::DapUsb::new(&mut builder);
    let control = CdcAcmClass::new(&mut builder, CTL_STATE.init(State::new()), PACKET as u16);
    let serial = CdcAcmClass::new(&mut builder, UART_STATE.init(State::new()), PACKET as u16);
    let mut device = builder.build();

    // One owner per direction and per peripheral side. Interrupt handlers feed
    // the async CAN and buffered-UART drivers; the tasks only move frames.
    let (host_tx, host_rx) = control.split();
    let (can_tx, can_rx, _properties) = ports.can.split();

    spawner.spawn(can_rx_task(can_rx).expect("CAN RX task slot"));
    spawner.spawn(can_tx_task(can_tx).expect("CAN TX task slot"));
    spawner.spawn(host_rx_task(host_rx, swd).expect("host RX task slot"));
    spawner.spawn(host_tx_task(host_tx).expect("host TX task slot"));

    join3(device.run(), dap.run(), uart_port(serial, ports.uart)).await;
}

fn bump(counter: &AtomicU32) {
    counter.fetch_add(1, Ordering::Relaxed);
}

async fn send_text(text: &str) {
    let reply = Reply::try_from(text).expect("fixed reply fits one short packet");
    send_reply(reply).await;
}

async fn send_reply(reply: Reply) {
    // A stalled host must not block the command parser forever. Drop the reply
    // after the bounded wait; the counter makes the loss visible via STAT.
    if with_timeout(Duration::from_millis(200), REPLIES.send(reply))
        .await
        .is_err()
    {
        bump(&DROP_TX);
    }
}

async fn handle_command(command: Result<HostCommand, ParseError>, swd: &mut Option<Swd>) {
    match command {
        Ok(HostCommand::Ping) => send_text("OK DM-MC02\n").await,
        Ok(HostCommand::SwdId) => match swd.as_mut() {
            Some(swd) => match swd.read_dpidr() {
                Some(id) => {
                    let mut out = Reply::new();
                    write!(out, "OK DPIDR {id:08X}\n").expect("SWD reply fits");
                    send_reply(out).await;
                }
                None => send_text("ERR SWD\n").await,
            },
            None => send_text("ERR SWD_NOT_CONFIGURED\n").await,
        },
        Ok(HostCommand::Tx(frame)) => {
            if HOST_TO_CAN.try_send(frame).is_ok() {
                send_text("OK TX\n").await;
            } else {
                // The CAN transmitter is keeping up less quickly than the host
                // produces requests. Report backpressure instead of blocking.
                bump(&DROP_TX);
                send_text("ERR BUSY\n").await;
            }
        }
        Ok(HostCommand::Stat) => {
            let line = protocol::status_line(
                RX_TOTAL.load(Ordering::Relaxed),
                TX_TOTAL.load(Ordering::Relaxed),
                DROP_RX.load(Ordering::Relaxed),
                DROP_TX.load(Ordering::Relaxed),
            );
            send_reply(line).await;
        }
        Ok(HostCommand::StreamRx) => send_text("ERR STREAM\n").await,
        Err(error) => {
            let mut out = Reply::new();
            write!(out, "ERR {}\n", error.as_str()).expect("error reply fits");
            send_reply(out).await;
        }
    }
}

/// Read one CDC-ACM OUT packet at a time and split it into newline-delimited
/// commands. A partial line survives until the next packet; a packet with
/// several newlines produces several commands.
#[embassy_executor::task]
async fn host_rx_task(mut usb: Receiver<'static, Usb>, mut swd: Option<Swd>) {
    let mut packet = [0; PACKET];
    loop {
        usb.wait_connection().await;
        let mut line = protocol::Line::new();
        while let Ok(n) = usb.read_packet(&mut packet).await {
            for &byte in &packet[..n] {
                if let Some(text) = line.push(byte) {
                    let command = protocol::parse(text);
                    handle_command(command, &mut swd).await;
                }
            }
        }
    }
}

/// Serialize every host-bound byte through a single CDC-ACM IN endpoint.
/// Replies have priority over queued CAN frames, and a write error drains both
/// queues so a reconnect never replays stale traffic.
#[embassy_executor::task]
async fn host_tx_task(mut usb: Sender<'static, Usb>) {
    // Boot-time traffic has no host yet; discard it once before the first
    // connection. Later disconnects are drained when a write fails, so a
    // command that races with connect is never dropped here.
    drain_host_queues();
    loop {
        usb.wait_connection().await;
        loop {
            let result = match next_host_item().await {
                HostTxItem::Reply(reply) => usb.write_packet(reply.as_bytes()).await,
                HostTxItem::Frame(frame) => {
                    let line = protocol::rx_line(&frame);
                    usb.write_packet(line.as_bytes()).await
                }
            };
            if result.is_err() {
                drain_host_queues();
                break;
            }
        }
    }
}

async fn next_host_item() -> HostTxItem {
    if let Ok(reply) = REPLIES.try_receive() {
        return HostTxItem::Reply(reply);
    }
    if let Ok(frame) = CAN_TO_HOST.try_receive() {
        return HostTxItem::Frame(frame);
    }
    match select(REPLIES.receive(), CAN_TO_HOST.receive()).await {
        Either::First(reply) => HostTxItem::Reply(reply),
        Either::Second(frame) => HostTxItem::Frame(frame),
    }
}

fn drain_host_queues() {
    while CAN_TO_HOST.try_receive().is_ok() {
        bump(&DROP_RX);
    }
    while REPLIES.try_receive().is_ok() {
        bump(&DROP_TX);
    }
}

/// CAN receive side: the FDCAN RX interrupt wakes this task, which forwards
/// standard data frames without waiting for a host request.
#[embassy_executor::task]
async fn can_rx_task(mut can: CanRx<'static>) {
    loop {
        match can.read().await {
            Ok(envelope) => {
                bump(&RX_TOTAL);
                match ClassicFrame::from_can(&envelope.frame) {
                    Some(frame) => {
                        if CAN_TO_HOST.try_send(frame).is_err() {
                            bump(&DROP_RX);
                        }
                    }
                    // Extended and remote frames are outside the declared
                    // Classical standard-data-frame scope.
                    None => bump(&DROP_RX),
                }
            }
            Err(_) => Timer::after_millis(1).await,
        }
    }
}

/// CAN transmit side: requests from the host are queued by `handle_command`
/// and written to the FDCAN TX mailboxes from this single owner task.
#[embassy_executor::task]
async fn can_tx_task(mut can: CanTx<'static>) {
    loop {
        let frame = HOST_TO_CAN.receive().await;
        let Some(frame) = frame.to_can() else {
            bump(&DROP_TX);
            continue;
        };
        // `write` may wait for a free mailbox. The bounded host queue turns
        // that backpressure into ERR BUSY; this timeout additionally keeps a
        // bus-off or unacknowledged bus from pinning the task forever.
        match with_timeout(Duration::from_millis(250), can.write(&frame)).await {
            Ok(None) => bump(&TX_TOTAL),
            // A lower-priority mailbox frame was replaced; the new frame is
            // queued, but the old one is a real loss.
            Ok(Some(_)) => {
                bump(&TX_TOTAL);
                bump(&DROP_TX);
            }
            Err(_) => bump(&DROP_TX),
        }
    }
}

async fn uart_port(usb: CdcAcmClass<'static, Usb>, uart: Option<BufferedUart<'static>>) {
    let Some(uart) = uart else {
        return;
    };
    let (mut serial_out, mut serial_in) = uart.split();
    let (mut usb_out, mut usb_in) = usb.split();
    loop {
        usb_in.wait_connection().await;
        let from_host = async {
            let mut buf = [0; PACKET];
            while let Ok(n) = usb_in.read_packet(&mut buf).await {
                if serial_out.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };
        let to_host = async {
            // A short packet needs no zero-length terminator.
            let mut buf = [0; PACKET - 1];
            while let Ok(n) = serial_in.read(&mut buf).await {
                if n != 0 && usb_out.write_packet(&buf[..n]).await.is_err() {
                    break;
                }
            }
        };
        select(from_host, to_host).await;
    }
}
