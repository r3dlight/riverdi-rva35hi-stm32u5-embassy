#![no_std]
#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

use embassy_stm32::{
    gpio::{Level, Output, Speed},
    pac,
    peripherals::{PB8, PD7, PE11, PE4},
};
use embedded_graphics::{
    pixelcolor::{raw::RawU16, Rgb565},
    prelude::*,
    primitives::Rectangle,
};

// ─── logging shim ────────────────────────────────────────────────────
//
// Driver is silent without the `defmt` feature; with it, it emits a
// couple of init lines through `defmt::info!`. No log fallback for
// now — most embedded users on stm32 use defmt anyway.

#[cfg(feature = "defmt")]
#[allow(unused_macros)]
macro_rules! info { ($($t:tt)*) => { ::defmt::info!($($t)*); }; }
#[cfg(not(feature = "defmt"))]
#[allow(unused_macros)]
macro_rules! info {
    ($($t:tt)*) => {};
}

// ─── public API ──────────────────────────────────────────────────────

/// Panel width in pixels after the landscape `MADCTL` setting baked
/// into the init sequence (`MADCTL = 0x7A`).
pub const PANEL_W: u16 = 480;

/// Panel height in pixels (companion to [`PANEL_W`]).
pub const PANEL_H: u16 = 320;

/// The four GPIO pins the driver takes ownership of.
///
/// Pin assignments are fixed by the Riverdi shield wiring — see the
/// crate-level README for the cross-reference with UM2861 Table 17.
pub struct DisplayPins {
    /// Data/command select. Wired to `FMC_A20`. **Must be `PE4`.**
    pub dc: PE4,
    /// Active-low panel reset. **Must be `PE11`.** See the gotcha
    /// note in the README about why this pin can't be left to FMC AF.
    pub rst: PE11,
    /// Backlight enable, active-high. Driven directly as GPIO.
    pub bl: PB8,
    /// FMC chip select. **Must be `PD7`** (`FMC_NE1`).
    pub cs: PD7,
}

/// Construction-time configuration for [`Display`].
///
/// Use [`Display::new`] for the defaults; reach for [`DisplayBuilder`]
/// when you need to override the HCLK frequency or opt out of the
/// MPU mapping that the driver normally puts over the FMC bank.
pub struct DisplayBuilder {
    pins: DisplayPins,
    hclk_hz: u32,
    mpu_ns_region: Option<u8>,
}

impl DisplayBuilder {
    /// Start a new builder with default settings:
    /// - `hclk_hz = 4_000_000` (matches `cortex-m-rt` boot defaults on
    ///   STM32U5 if you haven't reprogrammed RCC),
    /// - `mpu_ns_region = Some(4)` (map FMC bank as Device-nGnRnE so
    ///   the AHB matrix can't gather byte stores; see "MPU mapping"
    ///   in the README for why this is mandatory in practice).
    pub fn new(pins: DisplayPins) -> Self {
        Self {
            pins,
            hclk_hz: 4_000_000,
            mpu_ns_region: Some(4),
        }
    }

    /// Override the HCLK frequency the busy-wait delays in the reset
    /// + sleep-out path are calibrated against. Default `4_000_000`.
    ///
    /// Reset pulse holds RST low for `100 ms`, sleep-out delays for
    /// `120 ms`; both are realized via [`cortex_m::asm::delay`] which
    /// counts CPU cycles, so wrong `hclk_hz` ⇒ wrong delay duration.
    pub fn hclk_hz(mut self, hz: u32) -> Self {
        self.hclk_hz = hz;
        self
    }

    /// Choose which `MPU_NS` region to map over the FMC bank, or
    /// `None` to skip the MPU setup entirely. **If you pass `None`
    /// you are responsible for guaranteeing Device memory attributes
    /// on the FMC bank by other means**, otherwise the AHB matrix
    /// will gather byte writes and the panel will only show noise.
    pub fn mpu_ns_region(mut self, region: Option<u8>) -> Self {
        self.mpu_ns_region = region;
        self
    }

    /// Consume the builder and bring the panel up.
    pub fn build(self) -> Display {
        Display::build(self)
    }
}

/// ILI9488-on-FMC display handle.
///
/// Construct one with [`Display::new`] or [`DisplayBuilder::build`].
/// It owns the bus and the four shield pins for the rest of the run.
/// The panel is initialised, cleared to undefined contents, and ready
/// for [`DrawTarget`] calls or the fast-fill helpers ([`clear`](Self::clear),
/// [`fill_rect`](Self::fill_rect)).
///
/// There is no `Drop` — panel state is left enabled when the value
/// goes out of scope. In practice you build one at boot and keep it
/// alive forever inside an Embassy task or the main loop.
pub struct Display {
    bus: FmcBus,
    /// Backlight handle kept alive so embassy's `Drop` doesn't revert
    /// the GPIO to analog. Left high for the whole run.
    _bl: Output<'static>,
}

impl Display {
    /// Build a [`Display`] with default settings (`hclk_hz = 4 MHz`,
    /// MPU_NS region 4 over the FMC bank).
    ///
    /// Equivalent to `DisplayBuilder::new(pins).build()`. Use the
    /// builder when you need to deviate.
    pub fn new(pins: DisplayPins) -> Self {
        DisplayBuilder::new(pins).build()
    }

    fn build(cfg: DisplayBuilder) -> Self {
        let cycles_per_ms = cfg.hclk_hz / 1_000;

        let mut bl = Output::new(cfg.pins.bl, Level::High, Speed::Low);
        bl.set_high();

        Self::reset_pulse(cfg.pins.rst, cycles_per_ms);

        // Claim DC and CS so embassy doesn't hand them out elsewhere.
        // `configure_fmc_bus()` reconfigures them to AF12 (FMC_A20 and
        // FMC_NE1) below.
        let _dc = cfg.pins.dc;
        let _cs = cfg.pins.cs;

        unsafe {
            configure_fmc_bus(cfg.mpu_ns_region);
        }
        let bus = FmcBus;
        run_init_sequence(&bus, cycles_per_ms);

        Self { bus, _bl: bl }
    }

    /// Fill the entire panel with one [`Rgb565`] colour.
    pub fn clear(&self, color: Rgb565) {
        self.fill_rect_unchecked(0, 0, PANEL_W - 1, PANEL_H - 1, color);
    }

    /// Fill an axis-aligned rectangle with one colour.
    ///
    /// Out-of-range coords are clamped to the panel; the call is a
    /// no-op if the rectangle is empty or fully off-screen.
    pub fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: Rgb565) {
        if w == 0 || h == 0 {
            return;
        }
        let x1 = (x + w - 1).min(PANEL_W - 1);
        let y1 = (y + h - 1).min(PANEL_H - 1);
        if x > x1 || y > y1 {
            return;
        }
        self.fill_rect_unchecked(x, y, x1, y1, color);
    }
}

// ─── embedded-graphics integration ───────────────────────────────────

impl OriginDimensions for Display {
    fn size(&self) -> Size {
        Size::new(PANEL_W as u32, PANEL_H as u32)
    }
}

impl DrawTarget for Display {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, color) in pixels {
            if p.x < 0 || p.y < 0 {
                continue;
            }
            let x = p.x as u32;
            let y = p.y as u32;
            if x >= PANEL_W as u32 || y >= PANEL_H as u32 {
                continue;
            }
            let x = x as u16;
            let y = y as u16;
            self.set_window(x, y, x, y);
            let raw = RawU16::from(color).into_inner();
            self.bus.data((raw >> 8) as u8);
            self.bus.data(raw as u8);
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let Some(clipped) = area.intersection(&self.bounding_box()).bottom_right() else {
            return Ok(());
        };
        let tl = area.top_left.component_max(Point::zero());
        self.fill_rect_unchecked(
            tl.x as u16,
            tl.y as u16,
            clipped.x as u16,
            clipped.y as u16,
            color,
        );
        Ok(())
    }
}

// ─── optional Embassy task entry ─────────────────────────────────────

/// Minimal Embassy task: build a [`Display`] then idle forever.
///
/// Useful as a smoke-test that the LCD comes up. For anything
/// interactive, drop this and call [`Display::new`] from your own
/// task so you keep the handle.
///
/// Only available when the `embassy-task` feature is enabled.
#[cfg(feature = "embassy-task")]
#[embassy_executor::task]
pub async fn display_task(pins: DisplayPins) {
    info!("display_task: ILI9488 over FMC 8080 (A20=DC)");
    let _display = Display::new(pins);
    info!("display_task: panel up, idling");
    loop {
        embassy_time::Timer::after_secs(60).await;
    }
}

// ─── internals (bus, init, GPIO/FMC/MPU setup) ───────────────────────
//
// Private below this line. The public API above is the only intended
// surface.

/// 8-bit MCU 8080 link to the panel over FMC bank 1.
///
/// FMC runs in 16-bit MWID (see README "Three gotchas"), but the
/// panel only reads `DB[7:0]`, so the byte goes in the low half of
/// each `u16` write and the high byte is ignored. Bit 20 of the bus
/// address (`FMC_A20` = `PE4`) toggles LCD_RS, so cmd vs data is one
/// transaction with no GPIO bookkeeping.
struct FmcBus;

impl FmcBus {
    const CMD: *mut u16 = 0x6000_0000 as *mut u16;
    const DATA: *mut u16 = 0x6020_0000 as *mut u16;

    #[inline(always)]
    fn cmd(&self, code: u8) {
        unsafe {
            core::ptr::write_volatile(Self::CMD, code as u16);
            cortex_m::asm::dsb();
        }
    }

    #[inline(always)]
    fn data(&self, byte: u8) {
        unsafe {
            core::ptr::write_volatile(Self::DATA, byte as u16);
            cortex_m::asm::dsb();
        }
    }

    fn data_slice(&self, bytes: &[u8]) {
        for &b in bytes {
            self.data(b);
        }
    }

    /// Write `count` RGB565 pixels of `color`. Panel must be in
    /// pixel-data state (RAMWR sent).
    fn fill_pixels(&self, color: u16, count: u32) {
        let hi = (color >> 8) as u8;
        let lo = color as u8;
        for _ in 0..count {
            self.data(hi);
            self.data(lo);
        }
    }
}

impl Display {
    fn reset_pulse(rst: PE11, cycles_per_ms: u32) {
        let mut rst = Output::new(rst, Level::High, Speed::Low);
        rst.set_high();
        cortex_m::asm::delay(100 * cycles_per_ms);
        rst.set_low();
        cortex_m::asm::delay(100 * cycles_per_ms);
        rst.set_high();
        cortex_m::asm::delay(120 * cycles_per_ms);
        // PE11 must stay GPIO output high for the rest of the run —
        // its AF12 is FMC_D8 which would otherwise glitch LCD_RESET
        // on every FMC transaction. forget() stops embassy's Drop
        // from reverting MODER.
        core::mem::forget(rst);
    }

    fn set_window(&self, x0: u16, y0: u16, x1: u16, y1: u16) {
        self.bus.cmd(0x2A); // CASET
        self.bus
            .data_slice(&[(x0 >> 8) as u8, x0 as u8, (x1 >> 8) as u8, x1 as u8]);
        self.bus.cmd(0x2B); // PASET
        self.bus
            .data_slice(&[(y0 >> 8) as u8, y0 as u8, (y1 >> 8) as u8, y1 as u8]);
        self.bus.cmd(0x2C); // RAMWR
    }

    fn fill_rect_unchecked(&self, x0: u16, y0: u16, x1: u16, y1: u16, color: Rgb565) {
        self.set_window(x0, y0, x1, y1);
        let raw = RawU16::from(color).into_inner();
        let count = (x1 - x0 + 1) as u32 * (y1 - y0 + 1) as u32;
        self.bus.fill_pixels(raw, count);
    }
}

#[derive(Clone, Copy)]
enum GpioPort {
    D,
    E,
}

#[inline(always)]
unsafe fn rmw(addr: *mut u32, clear_mask: u32, set: u32) {
    let v = core::ptr::read_volatile(addr);
    core::ptr::write_volatile(addr, (v & !clear_mask) | set);
}

unsafe fn set_gpio_af(port: GpioPort, pins: &[u8], af: u8) {
    let base = match port {
        GpioPort::D => 0x4202_0C00usize,
        GpioPort::E => 0x4202_1000usize,
    };
    let moder = base as *mut u32;
    let ospeedr = (base + 0x08) as *mut u32;
    let pupdr = (base + 0x0C) as *mut u32;
    let afrl = (base + 0x20) as *mut u32;
    let afrh = (base + 0x24) as *mut u32;

    for &p in pins {
        let p = p as u32;
        rmw(moder, 0b11 << (p * 2), 0b10 << (p * 2)); // AF mode
        rmw(ospeedr, 0b11 << (p * 2), 0b11 << (p * 2)); // very high speed
        rmw(pupdr, 0b11 << (p * 2), 0); // no pull
        let af = (af as u32 & 0xF) << ((p & 7) * 4);
        let mask = 0xF << ((p & 7) * 4);
        let reg = if p < 8 { afrl } else { afrh };
        rmw(reg, mask, af);
    }
}

unsafe fn configure_fmc_bus(mpu_ns_region: Option<u8>) {
    pac::RCC.ahb2enr2().modify(|w| w.set_fsmcen(true));
    cortex_m::asm::dsb();

    // The 11 pins routed to actual LCD signals — verified against
    // the Riverdi schematic. PE11 is deliberately absent here so
    // its AF12 (= FMC_D8) doesn't fight the LCD_RESET wire.
    set_gpio_af(GpioPort::D, &[0, 1, 4, 5, 7, 14, 15], 12);
    set_gpio_af(GpioPort::E, &[4, 7, 8, 9, 10], 12);
    cortex_m::asm::dsb();
    cortex_m::asm::isb();

    widen_fmc_bcr1();
    if let Some(region) = mpu_ns_region {
        map_fmc_bank_device(region);
    }
}

unsafe fn widen_fmc_bcr1() {
    const BCR1: *mut u32 = 0x420D_0400 as *mut u32;
    let mut v = core::ptr::read_volatile(BCR1);
    core::ptr::write_volatile(BCR1, v & !1); // MBKEN=0 (required to change MWID)
    cortex_m::asm::dsb();
    v = (v & !(0b11 << 4)) | (0b01 << 4); // MWID = 01 = 16-bit on U5
    v |= 1; // MBKEN = 1
    core::ptr::write_volatile(BCR1, v);
    cortex_m::asm::dsb();
}

unsafe fn map_fmc_bank_device(region: u8) {
    const MPU_NS_CTRL: *mut u32 = 0xE000_ED94 as *mut u32;
    const MPU_NS_RNR: *mut u32 = 0xE000_ED98 as *mut u32;
    const MPU_NS_RBAR: *mut u32 = 0xE000_ED9C as *mut u32;
    const MPU_NS_RLAR: *mut u32 = 0xE000_EDA0 as *mut u32;
    const MPU_NS_MAIR0: *mut u32 = 0xE000_EDC0 as *mut u32;

    let ctrl_save = core::ptr::read_volatile(MPU_NS_CTRL);
    core::ptr::write_volatile(MPU_NS_CTRL, ctrl_save & !1); // disable MPU
    cortex_m::asm::dsb();
    cortex_m::asm::isb();

    // MAIR0 slot 3 ← Device-nGnRnE (0x00); slots 0/1/2 untouched.
    let mair0 = core::ptr::read_volatile(MPU_NS_MAIR0);
    core::ptr::write_volatile(MPU_NS_MAIR0, mair0 & 0x00FF_FFFF);

    core::ptr::write_volatile(MPU_NS_RNR, region as u32);
    core::ptr::write_volatile(MPU_NS_RBAR, 0x6000_0000 | (0b01 << 1) | 1); // RWAny, XN
    core::ptr::write_volatile(MPU_NS_RLAR, 0x6FFF_FFE0 | (3 << 1) | 1); // AttrIndx=3, EN

    core::ptr::write_volatile(MPU_NS_CTRL, ctrl_save | 1);
    cortex_m::asm::dsb();
    cortex_m::asm::isb();
}

/// Vendor ILI9488 init sequence, copied verbatim from Riverdi's
/// `riverdi-nucleo-3-50` reference (`Core/Src/display.c`). Panel-
/// specific gamma + `MADCTL=0x7A` (landscape, BGR) — substituting a
/// generic ILI9488 init makes colours come out wrong.
fn run_init_sequence(bus: &FmcBus, cycles_per_ms: u32) {
    // Positive gamma
    bus.cmd(0xE0);
    bus.data_slice(&[
        0x00, 0x10, 0x14, 0x01, 0x0E, 0x04, 0x33, 0x56, 0x48, 0x03, 0x0C, 0x0B, 0x2B, 0x34, 0x0F,
    ]);
    // Negative gamma
    bus.cmd(0xE1);
    bus.data_slice(&[
        0x00, 0x12, 0x18, 0x05, 0x12, 0x06, 0x40, 0x34, 0x57, 0x06, 0x10, 0x0C, 0x3B, 0x3F, 0x0F,
    ]);

    bus.cmd(0xC0);
    bus.data_slice(&[0x0F, 0x0C]); // Power 1
    bus.cmd(0xC1);
    bus.data(0x41); // Power 2
    bus.cmd(0xC5);
    bus.data_slice(&[0x00, 0x25, 0x80]); // VCOM

    bus.cmd(0x36);
    bus.data(0x7A); // MADCTL — landscape 480×320, BGR
    bus.cmd(0x3A);
    bus.data(0x55); // COLMOD — RGB565

    bus.cmd(0xB0);
    bus.data(0x00); // Interface mode
    bus.cmd(0xB1);
    bus.data(0xA0); // Frame rate
    bus.cmd(0xB4);
    bus.data(0x02); // Inversion
    bus.cmd(0xB6);
    bus.data_slice(&[0x02, 0x22]); // Display function

    bus.cmd(0x21); // Inversion ON (required on this IPS module)

    bus.cmd(0x11); // Sleep out
    cortex_m::asm::delay(120 * cycles_per_ms);

    bus.cmd(0x35);
    bus.data(0x00); // Tearing-effect line on
    bus.cmd(0x29); // Display on
}
