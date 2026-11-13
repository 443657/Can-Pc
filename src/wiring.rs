//! Complete the two TODOs in this file.

use embassy_stm32::{Peripherals, bind_interrupts, can::Can, usart::BufferedUart};

use crate::swd::Swd;
use embassy_stm32::gpio::{Output, Flex, Level, Speed};
pub struct Ports {
    pub can: Can<'static>,
    pub uart: Option<BufferedUart<'static>>,
}
bind_interrupts!(struct Irqs {
        UART7 => embassy_stm32::usart::BufferedInterruptHandler<embassy_stm32::peripherals::UART7>;
    }
);
static mut TX_BUF: [u8; 32] = [0; 32];
static mut RX_BUF: [u8; 32] = [0; 32];

pub fn configure(p: Peripherals) -> (crate::board::Usb, Ports, Option<Swd>) {
    let usb = crate::board::usb(p.USB_OTG_HS, p.PA12, p.PA11);

    // TODO 1: Pick two exposed 3.3 V GPIOs and pass them to Swd::new.
    // Use Output for SWCLK (initially low), and Flex for bidirectional SWDIO.
    let swclk = Output::new(p.PA0, Level::Low, Speed::Low);
    let swdio = Flex::new(p.PA2);
    let swd = Some(Swd::new(swclk,swdio));
    // TODO 2: Configure a buffered UART at 115200 baud, 8N1.
    // Identify the peripheral, RX/TX pins and interrupt from the schematic.
    // Bind the interrupt and provide static RX and TX buffers.
    use embassy_stm32::usart::Config;
    let mut uart_config = Config::default();
    uart_config.baudrate = 115200;
    let (tx_buf, rx_buf) = unsafe { (
    &mut *core::ptr::addr_of_mut!(TX_BUF),
    &mut *core::ptr::addr_of_mut!(RX_BUF)
) };
    let uart = Some(BufferedUart::new(
        p.UART7,
        
        p.PE7,
        p.PE8,
         rx_buf,
         tx_buf,
        Irqs,
        uart_config
    ).unwrap());
    let can = crate::board::can1(p.FDCAN1, p.PD0, p.PD1);
    (usb, Ports { can, uart }, swd)
}
